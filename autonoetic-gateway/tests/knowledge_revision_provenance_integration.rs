use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_types::agent_revision::{
    AgentAliasRecord, AgentRevisionRecord, AgentRevisionStatus, SessionAgentBinding,
};
use autonoetic_types::memory::{MemoryObject, MemoryVisibility};
use autonoetic_types::principal::PrincipalKind;
use std::sync::Arc;
use tempfile::tempdir;

fn make_gateway_dir(tmp: &tempfile::TempDir) -> std::path::PathBuf {
    let gw = tmp.path().join(".gateway");
    std::fs::create_dir_all(&gw).unwrap();
    gw
}

fn seed_revision(store: &GatewayStore, agent_id: &str, revision_id: &str) {
    store
        .insert_agent_revision(&AgentRevisionRecord {
            revision_id: revision_id.to_string(),
            agent_id: agent_id.to_string(),
            base_revision_id: None,
            artifact_id: None,
            content_digest: format!("sha256:{revision_id}"),
            runtime_lock_hash: "sha256:test-lock".to_string(),
            manifest_hash: "sha256:test-manifest".to_string(),
            created_at: chrono::Utc::now().to_rfc3339(),
            created_by_type: PrincipalKind::Human.tag().to_string(),
            created_by_id: "test".to_string(),
            source_kind: "test".to_string(),
            source_ref: None,
            origin_node_id: "gateway".to_string(),
            trust_domain: "local".to_string(),
            status: AgentRevisionStatus::Ready,
            metadata_json: serde_json::json!({}),
            short_id: String::new(),
            signature: None,
            signer_id: None,
        })
        .unwrap();
}

fn seed_alias(store: &GatewayStore, agent_id: &str, revision_id: &str) {
    store
        .upsert_agent_alias(&AgentAliasRecord {
            alias_id: agent_id.to_string(),
            agent_id: agent_id.to_string(),
            revision_id: revision_id.to_string(),
            updated_at: chrono::Utc::now().to_rfc3339(),
            updated_by_type: PrincipalKind::Human.tag().to_string(),
            updated_by_id: "test".to_string(),
            reason: Some("test".to_string()),
            suspended_at: None,
            suspended_reason: None,
            suspended_by: None,
        })
        .unwrap();
}

fn make_memory(id: &str, scope: &str, agent_id: &str, revision_id: Option<&str>) -> MemoryObject {
    let mut mem = MemoryObject::new(
        id.to_string(),
        scope.to_string(),
        agent_id.to_string(),
        agent_id.to_string(),
        "session:test:turn:1".to_string(),
        format!("content for {id}"),
    );
    mem.visibility = MemoryVisibility::Global;
    mem.revision_id = revision_id.map(|s| s.to_string());
    mem
}

#[test]
fn test_memory_revision_id_stored_and_retrieved() {
    let tmp = tempdir().unwrap();
    let gw = make_gateway_dir(&tmp);
    let store = GatewayStore::open(&gw).unwrap();

    let mem = make_memory("fact1", "general", "agent-a", Some("rev_001"));
    store.memory_upsert(&mem).unwrap();

    let loaded = store.memory_get_unrestricted("fact1").unwrap().unwrap();
    assert_eq!(loaded.revision_id.as_deref(), Some("rev_001"));
    assert!(loaded.binding_session_id.is_none());
    assert!(loaded.alias_ref.is_none());
    assert!(loaded.quarantine_reason.is_none());
}

#[test]
fn test_memory_with_binding_fields_stored() {
    let tmp = tempdir().unwrap();
    let gw = make_gateway_dir(&tmp);
    let store = GatewayStore::open(&gw).unwrap();

    let mut mem = make_memory("fact2", "lessons", "agent-b", Some("rev_002"));
    mem.binding_session_id = Some("sess_abc".to_string());
    mem.alias_ref = Some("agent-b".to_string());
    store.memory_upsert(&mem).unwrap();

    let loaded = store.memory_get_unrestricted("fact2").unwrap().unwrap();
    assert_eq!(loaded.revision_id.as_deref(), Some("rev_002"));
    assert_eq!(loaded.binding_session_id.as_deref(), Some("sess_abc"));
    assert_eq!(loaded.alias_ref.as_deref(), Some("agent-b"));
}

#[test]
fn test_quarantined_memories_excluded_from_search() {
    let tmp = tempdir().unwrap();
    let gw = make_gateway_dir(&tmp);
    let store = GatewayStore::open(&gw).unwrap();

    let mem_normal = make_memory("fact_ok", "scope_a", "agent-a", Some("rev_good"));
    store.memory_upsert(&mem_normal).unwrap();

    let mut mem_quarantined = make_memory("fact_bad", "scope_a", "agent-a", Some("rev_bad"));
    mem_quarantined.quarantine_reason = Some("revision_rollback:rev_bad".to_string());
    store.memory_upsert(&mem_quarantined).unwrap();

    let ids = store.memory_list_ids_for_scope("scope_a", None).unwrap();
    assert_eq!(ids.len(), 1);
    assert_eq!(ids[0], "fact_ok");

    let quarantined = store.memory_get_unrestricted("fact_bad").unwrap().unwrap();
    assert!(quarantined.quarantine_reason.is_some());
}

#[test]
fn test_quarantined_memories_excluded_from_owned_list() {
    let tmp = tempdir().unwrap();
    let gw = make_gateway_dir(&tmp);
    let store = GatewayStore::open(&gw).unwrap();

    let mem_normal = make_memory("owned_ok", "scope_b", "agent-x", Some("rev_x1"));
    store.memory_upsert(&mem_normal).unwrap();

    let mut mem_quarantined = make_memory("owned_bad", "scope_b", "agent-x", Some("rev_x2"));
    mem_quarantined.quarantine_reason = Some("test_quarantine".to_string());
    store.memory_upsert(&mem_quarantined).unwrap();

    let ids = store.memory_list_ids_owned_by("agent-x").unwrap();
    assert_eq!(ids.len(), 1);
    assert_eq!(ids[0], "owned_ok");
}

#[test]
fn test_quarantined_memories_excluded_from_scopes_list() {
    let tmp = tempdir().unwrap();
    let gw = make_gateway_dir(&tmp);
    let store = GatewayStore::open(&gw).unwrap();

    let mut mem_global = make_memory("g1", "scope_g", "agent-g", Some("rev_g1"));
    mem_global.visibility = MemoryVisibility::Global;
    store.memory_upsert(&mem_global).unwrap();

    let mut mem_quarantined = make_memory("g2", "scope_gq", "agent-g", Some("rev_g2"));
    mem_quarantined.visibility = MemoryVisibility::Global;
    mem_quarantined.quarantine_reason = Some("quarantined".to_string());
    store.memory_upsert(&mem_quarantined).unwrap();

    let scopes = store.memory_list_scopes_for_agent("agent-g", None).unwrap();
    assert!(scopes.contains(&"scope_g".to_string()));
    assert!(!scopes.contains(&"scope_gq".to_string()));
}

#[test]
fn test_memory_quarantine_by_revision() {
    let tmp = tempdir().unwrap();
    let gw = make_gateway_dir(&tmp);
    let store = GatewayStore::open(&gw).unwrap();

    store
        .memory_upsert(&make_memory("m1", "s", "a", Some("rev_A")))
        .unwrap();
    store
        .memory_upsert(&make_memory("m2", "s", "a", Some("rev_A")))
        .unwrap();
    store
        .memory_upsert(&make_memory("m3", "s", "a", Some("rev_B")))
        .unwrap();

    let count = store
        .memory_quarantine_by_revision("rev_A", "rollback:test")
        .unwrap();
    assert_eq!(count, 2);

    let quarantined = store.memory_list_quarantined_for_revision("rev_A").unwrap();
    assert_eq!(quarantined.len(), 2);

    let m1 = store.memory_get_unrestricted("m1").unwrap().unwrap();
    assert_eq!(m1.quarantine_reason.as_deref(), Some("rollback:test"));

    let m3 = store.memory_get_unrestricted("m3").unwrap().unwrap();
    assert!(m3.quarantine_reason.is_none());
}

#[test]
fn test_atomic_rollback_quarantines_knowledge() {
    let tmp = tempdir().unwrap();
    let gw = make_gateway_dir(&tmp);
    let store = GatewayStore::open(&gw).unwrap();
    let agent_id = "test-agent";

    seed_revision(&store, agent_id, "rev_old");
    seed_revision(&store, agent_id, "rev_new");
    seed_alias(&store, agent_id, "rev_new");

    let mut mem_old = make_memory("old_fact", "scope1", agent_id, Some("rev_old"));
    mem_old.visibility = MemoryVisibility::Global;
    store.memory_upsert(&mem_old).unwrap();

    let mut mem_new = make_memory("new_fact", "scope1", agent_id, Some("rev_new"));
    mem_new.visibility = MemoryVisibility::Global;
    store.memory_upsert(&mem_new).unwrap();

    let mut mem_new2 = make_memory("new_fact2", "scope2", agent_id, Some("rev_new"));
    mem_new2.visibility = MemoryVisibility::Global;
    store.memory_upsert(&mem_new2).unwrap();

    let previous = store
        .atomic_rollback(
            agent_id,
            "rev_old",
            "promo_rollback_1",
            "test",
            "test",
            Some("reverting bad deploy"),
        )
        .unwrap();
    assert_eq!(previous.as_deref(), Some("rev_new"));

    let m_new = store.memory_get_unrestricted("new_fact").unwrap().unwrap();
    assert!(m_new.quarantine_reason.is_some());
    assert!(m_new.quarantine_reason.unwrap().contains("rev_new"));

    let m_new2 = store.memory_get_unrestricted("new_fact2").unwrap().unwrap();
    assert!(m_new2.quarantine_reason.is_some());

    let m_old = store.memory_get_unrestricted("old_fact").unwrap().unwrap();
    assert!(m_old.quarantine_reason.is_none());

    let ids = store.memory_list_ids_for_scope("scope1", None).unwrap();
    assert_eq!(ids.len(), 1);
    assert_eq!(ids[0], "old_fact");

    let alias = store.get_agent_alias(agent_id).unwrap().unwrap();
    assert_eq!(alias.revision_id, "rev_old");
}

#[test]
fn test_atomic_rollback_noop_when_same_revision() {
    let tmp = tempdir().unwrap();
    let gw = make_gateway_dir(&tmp);
    let store = GatewayStore::open(&gw).unwrap();
    let agent_id = "agent-same";

    seed_revision(&store, agent_id, "rev_only");
    seed_alias(&store, agent_id, "rev_only");

    let mut mem = make_memory("only_fact", "s", agent_id, Some("rev_only"));
    mem.visibility = MemoryVisibility::Global;
    store.memory_upsert(&mem).unwrap();

    let previous = store
        .atomic_rollback(agent_id, "rev_only", "promo_noop", "test", "test", None)
        .unwrap();
    assert_eq!(previous.as_deref(), Some("rev_only"));

    let m = store.memory_get_unrestricted("only_fact").unwrap().unwrap();
    assert!(m.quarantine_reason.is_none());
}

#[test]
fn test_session_binding_tags_knowledge_store_write() {
    let tmp = tempdir().unwrap();
    let gw = make_gateway_dir(&tmp);
    let store = Arc::new(GatewayStore::open(&gw).unwrap());

    let agent_id = "bound-agent";
    let revision_id = "rev_bound_001";
    let session_id = "sess_123";
    let alias_id = "alias_bound";

    seed_revision(&store, agent_id, revision_id);
    seed_alias(&store, agent_id, revision_id);

    store
        .upsert_session_agent_binding(&SessionAgentBinding {
            session_id: session_id.to_string(),
            root_session_id: session_id.to_string(),
            alias_id: Some(alias_id.to_string()),
            agent_id: agent_id.to_string(),
            revision_id: revision_id.to_string(),
            runtime_lock_hash: "sha256:test".to_string(),
            home_node_id: "gateway".to_string(),
            created_at: chrono::Utc::now().to_rfc3339(),
            requested_target: agent_id.to_string(),
        })
        .unwrap();

    let binding = store
        .get_session_agent_binding(session_id)
        .unwrap()
        .unwrap();
    assert_eq!(binding.revision_id, revision_id);
    assert_eq!(binding.alias_id.as_deref(), Some(alias_id));

    let mut mem = make_memory("bound_fact", "scope_bound", agent_id, None);
    if let Some(binding) = store.get_session_agent_binding(session_id).unwrap() {
        mem.revision_id = Some(binding.revision_id.clone());
        mem.binding_session_id = Some(binding.session_id.clone());
        mem.alias_ref = binding.alias_id.clone();
    }
    mem.visibility = MemoryVisibility::Global;
    store.memory_upsert(&mem).unwrap();

    let loaded = store
        .memory_get_unrestricted("bound_fact")
        .unwrap()
        .unwrap();
    assert_eq!(loaded.revision_id.as_deref(), Some(revision_id));
    assert_eq!(loaded.binding_session_id.as_deref(), Some(session_id));
    assert_eq!(loaded.alias_ref.as_deref(), Some(alias_id));
}

#[test]
fn test_quarantine_dedup_no_double_quarantine() {
    let tmp = tempdir().unwrap();
    let gw = make_gateway_dir(&tmp);
    let store = GatewayStore::open(&gw).unwrap();

    let mut mem = make_memory("dedup_fact", "s", "a", Some("rev_dedup"));
    mem.quarantine_reason = Some("already_quarantined".to_string());
    store.memory_upsert(&mem).unwrap();

    let count = store
        .memory_quarantine_by_revision("rev_dedup", "second_quarantine")
        .unwrap();
    assert_eq!(count, 0);

    let loaded = store
        .memory_get_unrestricted("dedup_fact")
        .unwrap()
        .unwrap();
    assert_eq!(
        loaded.quarantine_reason.as_deref(),
        Some("already_quarantined")
    );
}

#[test]
fn test_memory_get_excludes_quarantined() {
    let tmp = tempdir().unwrap();
    let gw = make_gateway_dir(&tmp);
    let store = GatewayStore::open(&gw).unwrap();

    let mem_normal = make_memory("get_ok", "s", "a", Some("rev_ok"));
    store.memory_upsert(&mem_normal).unwrap();

    let mut mem_quarantined = make_memory("get_bad", "s", "a", Some("rev_bad"));
    mem_quarantined.quarantine_reason = Some("quarantined".to_string());
    store.memory_upsert(&mem_quarantined).unwrap();

    assert!(store.memory_get("get_ok").unwrap().is_some());
    assert!(store.memory_get("get_bad").unwrap().is_none());
    assert!(store.memory_get_unrestricted("get_bad").unwrap().is_some());
}
