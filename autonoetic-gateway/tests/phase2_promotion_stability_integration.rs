use autonoetic_gateway::agent::repository::AgentRepository;
use autonoetic_gateway::policy::PolicyEngine;
use autonoetic_gateway::runtime::tools::default_registry;
use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_types::agent::{AgentIdentity, AgentManifest, RuntimeDeclaration};
use autonoetic_types::agent_revision::{
    AgentAliasRecord, AgentRevisionRecord, AgentRevisionStatus,
};
use autonoetic_types::capability::Capability;
use serde_json::json;
use std::path::Path;
use std::sync::Arc;
use tempfile::TempDir;

fn revision_manifest() -> AgentManifest {
    AgentManifest {
        version: "1.0".to_string(),
        runtime: RuntimeDeclaration {
            engine: "autonoetic".to_string(),
            gateway_version: "0.1.0".to_string(),
            sdk_version: "0.1.0".to_string(),
            runtime_type: "stateful".to_string(),
            sandbox: "bubblewrap".to_string(),
            runtime_lock: "runtime.lock".to_string(),
        },
        agent: AgentIdentity {
            id: "test-promoter".to_string(),
            name: "test-promoter".to_string(),
            description: "Test promotion agent".to_string(),
        },
        capabilities: vec![Capability::AgentRevision {
            patterns: vec!["*".to_string()],
        }],
        llm_config: None,
        limits: None,
        background: None,
        disclosure: None,
        io: None,
        middleware: None,
        execution_mode: Default::default(),
        script_entry: None,
        script_input_mode: Default::default(),
        gateway_url: None,
        gateway_token: None,
        response_contract: None,
        allowed_tool_tiers: vec![],
        agentskills_import: None,
        compression: None,
    }
}

fn make_revision(agent_id: &str, suffix: &str) -> AgentRevisionRecord {
    AgentRevisionRecord {
        revision_id: format!("rev_sha256:{suffix}"),
        agent_id: agent_id.to_string(),
        base_revision_id: None,
        artifact_id: None,
        content_digest: format!("sha256:content-{suffix}"),
        runtime_lock_hash: format!("sha256:runtime-{suffix}"),
        manifest_hash: format!("sha256:manifest-{suffix}"),
        created_at: chrono::Utc::now().to_rfc3339(),
        created_by_type: "test".to_string(),
        created_by_id: "test".to_string(),
        source_kind: "test".to_string(),
        source_ref: None,
        origin_node_id: "gateway".to_string(),
        trust_domain: "local".to_string(),
        status: AgentRevisionStatus::Candidate,
        metadata_json: json!({}),
        short_id: format!("sid{}", &suffix[..8]),
        signature: None,
        signer_id: None,
    }
}

fn upsert_alias(store: &GatewayStore, agent_id: &str, revision_id: &str, reason: &str) {
    let alias = AgentAliasRecord {
        alias_id: agent_id.to_string(),
        agent_id: agent_id.to_string(),
        revision_id: revision_id.to_string(),
        updated_at: chrono::Utc::now().to_rfc3339(),
        updated_by_type: "test".to_string(),
        updated_by_id: "test".to_string(),
        reason: Some(reason.to_string()),
    };
    store.upsert_agent_alias(&alias).unwrap();
}

fn setup_store_and_repo() -> (TempDir, Arc<GatewayStore>, AgentRepository) {
    let tmp = TempDir::new().unwrap();
    let gateway_dir = tmp.path().join(".gateway");
    std::fs::create_dir_all(&gateway_dir).unwrap();
    let store = Arc::new(GatewayStore::open(&gateway_dir).unwrap());
    let repo = AgentRepository::new(tmp.path().join("agents"));
    (tmp, store, repo)
}

/// Creates a minimal SKILL.md in the revision directory so that the
/// promotion gate can read capabilities (needed since the hardening changes).
fn materialize_revision_skill(gateway_dir: &Path, agent_id: &str, revision_id: &str) {
    let revision_dir = gateway_dir
        .join("revisions/agents")
        .join(agent_id)
        .join(revision_id);
    std::fs::create_dir_all(&revision_dir).unwrap();
    std::fs::write(
        revision_dir.join("SKILL.md"),
        format!(
            "---\nversion: \"1.0\"\nagent:\n  id: \"{agent_id}\"\n  name: \"{agent_id}\"\n  description: test\ncapabilities: []\n---\n# Test\n"
        ),
    )
    .unwrap();
}

#[test]
fn test_promote_changes_only_future_alias_resolution_and_running_session_stays_pinned() {
    let (_tmp, store, repo) = setup_store_and_repo();
    let agent_id = "planner.default";
    let rev1 = make_revision(
        agent_id,
        "1111111111111111111111111111111111111111111111111111111111111111",
    );
    let rev2 = make_revision(
        agent_id,
        "2222222222222222222222222222222222222222222222222222222222222222",
    );
    store.insert_agent_revision(&rev1).unwrap();
    store.insert_agent_revision(&rev2).unwrap();
    upsert_alias(store.as_ref(), agent_id, &rev1.revision_id, "initial");

    let (initial_ref, _, _) = repo
        .resolve_and_pin_session(
            "session-running",
            "session-running",
            agent_id,
            Some(store.as_ref()),
            "host:test",
        )
        .unwrap();
    assert_eq!(initial_ref.revision_id, rev1.revision_id);

    store
        .atomic_promote(
            agent_id,
            &rev2.revision_id,
            "prom-test",
            "agent",
            "test-promoter",
            Some("promote for test"),
            None,
        )
        .unwrap();

    let alias = store.resolve_alias(agent_id).unwrap().unwrap();
    assert_eq!(alias.revision_id, rev2.revision_id);

    let (future_ref, _, _) = repo
        .resolve_and_pin_session(
            "session-future",
            "session-future",
            agent_id,
            Some(store.as_ref()),
            "host:test",
        )
        .unwrap();
    assert_eq!(future_ref.revision_id, rev2.revision_id);

    let (running_ref_again, _, _) = repo
        .resolve_and_pin_session(
            "session-running",
            "session-running",
            agent_id,
            Some(store.as_ref()),
            "host:test",
        )
        .unwrap();
    assert_eq!(running_ref_again.revision_id, rev1.revision_id);
}

#[test]
fn test_explicit_agent_ref_sessions_are_unaffected_by_later_promotion() {
    let (_tmp, store, repo) = setup_store_and_repo();
    let agent_id = "planner.default";
    let rev1 = make_revision(
        agent_id,
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
    );
    let rev2 = make_revision(
        agent_id,
        "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
    );
    store.insert_agent_revision(&rev1).unwrap();
    store.insert_agent_revision(&rev2).unwrap();
    upsert_alias(store.as_ref(), agent_id, &rev1.revision_id, "initial");

    let explicit_target = format!("{agent_id}@{}", rev1.revision_id);
    let (explicit_before, _, _) = repo
        .resolve_and_pin_session(
            "session-explicit-before",
            "session-explicit-before",
            &explicit_target,
            Some(store.as_ref()),
            "host:test",
        )
        .unwrap();
    assert_eq!(explicit_before.revision_id, rev1.revision_id);

    store
        .atomic_promote(
            agent_id,
            &rev2.revision_id,
            "prom-test-explicit",
            "agent",
            "test-promoter",
            Some("promote for explicit test"),
            None,
        )
        .unwrap();

    let (explicit_after, _, _) = repo
        .resolve_and_pin_session(
            "session-explicit-after",
            "session-explicit-after",
            &explicit_target,
            Some(store.as_ref()),
            "host:test",
        )
        .unwrap();
    assert_eq!(explicit_after.revision_id, rev1.revision_id);

    let (alias_after, _, _) = repo
        .resolve_and_pin_session(
            "session-alias-after",
            "session-alias-after",
            agent_id,
            Some(store.as_ref()),
            "host:test",
        )
        .unwrap();
    assert_eq!(alias_after.revision_id, rev2.revision_id);
}

#[test]
fn test_rollback_restores_previous_alias_target() {
    let (tmp, store, _repo) = setup_store_and_repo();
    let agent_id = "planner.default";
    let rev1 = make_revision(
        agent_id,
        "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc",
    );
    let rev2 = make_revision(
        agent_id,
        "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd",
    );
    store.insert_agent_revision(&rev1).unwrap();
    store.insert_agent_revision(&rev2).unwrap();
    upsert_alias(store.as_ref(), agent_id, &rev1.revision_id, "initial");

    // Materialize SKILL.md for both revisions so the promotion gate can read capabilities.
    let gateway_dir = tmp.path().join(".gateway");
    materialize_revision_skill(&gateway_dir, agent_id, &rev1.revision_id);
    materialize_revision_skill(&gateway_dir, agent_id, &rev2.revision_id);

    let registry = default_registry();
    let manifest = revision_manifest();
    let policy = PolicyEngine::new(manifest.clone());

    registry
        .execute(
            "agent_revision_promote",
            &manifest,
            &policy,
            tmp.path(),
            Some(&gateway_dir),
            &json!({
                "agent_id": agent_id,
                "revision_id": rev2.revision_id,
                "reason": "promote in rollback test",
            })
            .to_string(),
            Some("session-promote"),
            Some("turn-1"),
            None,
            Some(store.clone()),
            None,
        )
        .unwrap();

    registry
        .execute(
            "agent_revision_rollback",
            &manifest,
            &policy,
            Path::new("."),
            Some(&tmp.path().join(".gateway")),
            &json!({
                "agent_id": agent_id,
                "reason": "rollback to previous"
            })
            .to_string(),
            Some("session-rollback"),
            Some("turn-2"),
            None,
            Some(store.clone()),
            None,
        )
        .unwrap();

    let alias = store.resolve_alias(agent_id).unwrap().unwrap();
    assert_eq!(alias.revision_id, rev1.revision_id);
}

#[test]
fn test_promotion_fails_for_mismatched_alias_and_agent() {
    let (tmp, store, _repo) = setup_store_and_repo();
    let agent_a = "planner.default";
    let agent_b = "coder.default";
    let rev_a = make_revision(
        agent_a,
        "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee",
    );
    let rev_b = make_revision(
        agent_b,
        "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff",
    );
    store.insert_agent_revision(&rev_a).unwrap();
    store.insert_agent_revision(&rev_b).unwrap();
    upsert_alias(store.as_ref(), agent_a, &rev_a.revision_id, "initial");

    let registry = default_registry();
    let manifest = revision_manifest();
    let policy = PolicyEngine::new(manifest.clone());

    let err = registry
        .execute(
            "agent_revision_promote",
            &manifest,
            &policy,
            tmp.path(),
            Some(&tmp.path().join(".gateway")),
            &json!({
                "agent_id": agent_a,
                "revision_id": rev_b.revision_id,
                "reason": "should fail",
            })
            .to_string(),
            Some("session-mismatch"),
            Some("turn-1"),
            None,
            Some(store.clone()),
            None,
        )
        .expect_err("promotion should fail for mismatched alias/agent");

    assert!(
        err.to_string().contains("belongs to agent"),
        "unexpected error: {err}"
    );
}
