//! Integration tests: artifact refs created in a parent session resolve from a
//! forked session via `session_fork_lineage`.

use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_types::artifact::{ArtifactRefRecord, ArtifactRefScopeType};
use tempfile::tempdir;

fn make_ref(
    ref_id: &str,
    scope_type: ArtifactRefScopeType,
    scope_id: &str,
    artifact_id: &str,
) -> ArtifactRefRecord {
    ArtifactRefRecord {
        ref_id: ref_id.to_string(),
        scope_type,
        scope_id: scope_id.to_string(),
        artifact_id: artifact_id.to_string(),
        artifact_manifest_digest:
            "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".to_string(),
        artifact_canonical_digest:
            "sha256:fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210".to_string(),
        created_by_agent_id: "planner.default".to_string(),
        created_at: chrono::Utc::now().to_rfc3339(),
        expires_at: None,
        revoked_at: None,
    }
}

#[test]
fn forked_session_resolves_parent_session_scoped_ref() -> anyhow::Result<()> {
    let temp = tempdir()?;
    let store = GatewayStore::open(temp.path())?;

    // Parent session creates an artifact ref scoped to its root session.
    let parent_session = "sess-parent";
    store.create_artifact_ref(&make_ref(
        "ar.38b1ae3f2c28",
        ArtifactRefScopeType::Session,
        parent_session,
        "art_parent",
    ))?;

    // Without fork lineage, the fork can't see it.
    let fork_session = "fork-abc";
    let unresolved = store.resolve_artifact_ref_any_scope("ar.38b1ae3f2c28", fork_session)?;
    assert!(unresolved.is_none(), "ref should not resolve before lineage is recorded");

    // Record fork lineage.
    store.record_fork_lineage(fork_session, parent_session, 0, None, "test-agent")?;

    // Now the fork resolves the parent's ref.
    let resolved = store
        .resolve_artifact_ref_any_scope("ar.38b1ae3f2c28", fork_session)?
        .expect("ref should resolve via fork lineage");
    assert_eq!(resolved.artifact_id, "art_parent");
    assert_eq!(resolved.scope_id, parent_session);

    Ok(())
}

#[test]
fn forked_session_resolves_parent_ref_via_root() -> anyhow::Result<()> {
    let temp = tempdir()?;
    let store = GatewayStore::open(temp.path())?;

    // Parent is a root session; artifact scoped to the root.
    let parent_root = "sess-root";
    store.create_artifact_ref(&make_ref(
        "ar.root1",
        ArtifactRefScopeType::Session,
        parent_root,
        "art_root",
    ))?;

    // A child session of the fork: fork-root/child
    // Fork lineage: fork-root → sess-root
    let fork_root = "fork-xyz";
    let child = "fork-xyz/T5";

    store.record_fork_lineage(fork_root, parent_root, 0, None, "test-agent")?;

    // The child resolves through its own root (fork-xyz) which has no ref,
    // then walks the fork lineage to sess-root.
    let resolved = store
        .resolve_artifact_ref_any_scope("ar.root1", child)?
        .expect("child should resolve via root → fork lineage");
    assert_eq!(resolved.artifact_id, "art_root");

    Ok(())
}

#[test]
fn fork_of_fork_resolves_grandparent_ref() -> anyhow::Result<()> {
    let temp = tempdir()?;
    let store = GatewayStore::open(temp.path())?;

    let grandparent = "sess-gp";
    store.create_artifact_ref(&make_ref(
        "ar.gp1",
        ArtifactRefScopeType::Session,
        grandparent,
        "art_gp",
    ))?;

    // fork1 → grandparent
    let fork1 = "fork-level1";
    store.record_fork_lineage(fork1, grandparent, 0, None, "test-agent")?;

    // fork2 → fork1 (fork of a fork)
    let fork2 = "fork-level2";
    store.record_fork_lineage(fork2, fork1, 0, None, "test-agent")?;

    // fork2 walks fork1 → grandparent to find the ref.
    let resolved = store
        .resolve_artifact_ref_any_scope("ar.gp1", fork2)?
        .expect("fork-of-fork should resolve grandparent ref");
    assert_eq!(resolved.artifact_id, "art_gp");

    Ok(())
}

#[test]
fn fork_does_not_resolve_unrelated_session_ref() -> anyhow::Result<()> {
    let temp = tempdir()?;
    let store = GatewayStore::open(temp.path())?;

    let parent = "sess-real-parent";
    let stranger = "sess-unrelated";
    store.create_artifact_ref(&make_ref(
        "ar.stranger",
        ArtifactRefScopeType::Session,
        stranger,
        "art_stranger",
    ))?;

    let fork = "fork-orphan";
    store.record_fork_lineage(fork, parent, 0, None, "test-agent")?;

    // The fork's lineage is parent (sess-real-parent), not stranger.
    let unresolved = store.resolve_artifact_ref_any_scope("ar.stranger", fork)?;
    assert!(unresolved.is_none(), "fork must not resolve refs from unrelated sessions");

    Ok(())
}

#[test]
fn find_active_ref_for_artifact_walks_fork_lineage() -> anyhow::Result<()> {
    let temp = tempdir()?;
    let store = GatewayStore::open(temp.path())?;

    let parent = "sess-parent-find";
    store.create_artifact_ref(&make_ref(
        "ar.find1",
        ArtifactRefScopeType::Session,
        parent,
        "art_find",
    ))?;

    let fork = "fork-find";
    store.record_fork_lineage(fork, parent, 0, None, "test-agent")?;

    // find_active_ref_for_artifact should find ar.find1 via fork lineage.
    let found = store
        .find_active_ref_for_artifact("art_find", fork)?
        .expect("should find ref via fork lineage");
    assert_eq!(found, "ar.find1");

    Ok(())
}

#[test]
fn expired_parent_ref_not_resolved_from_fork() -> anyhow::Result<()> {
    let temp = tempdir()?;
    let store = GatewayStore::open(temp.path())?;

    let parent = "sess-parent-exp";
    let mut record = make_ref(
        "ar.expired1",
        ArtifactRefScopeType::Session,
        parent,
        "art_expired",
    );
    record.expires_at = Some((chrono::Utc::now() - chrono::Duration::seconds(60)).to_rfc3339());
    store.create_artifact_ref(&record)?;

    let fork = "fork-exp";
    store.record_fork_lineage(fork, parent, 0, None, "test-agent")?;

    let unresolved = store.resolve_artifact_ref_any_scope("ar.expired1", fork)?;
    assert!(unresolved.is_none(), "expired parent ref must not resolve from fork");

    Ok(())
}

#[test]
fn no_fork_lineage_no_resolution() -> anyhow::Result<()> {
    let temp = tempdir()?;
    let store = GatewayStore::open(temp.path())?;

    let parent = "sess-nolineage";
    store.create_artifact_ref(&make_ref(
        "ar.nolink",
        ArtifactRefScopeType::Session,
        parent,
        "art_nolink",
    ))?;

    let fork = "fork-unlinked";
    // No record_fork_lineage call.

    let unresolved = store.resolve_artifact_ref_any_scope("ar.nolink", fork)?;
    assert!(unresolved.is_none(), "without lineage, fork must not resolve parent refs");

    let source = store.get_fork_source(fork)?;
    assert!(source.is_none(), "get_fork_source should return None for non-fork");

    Ok(())
}

#[test]
fn backfill_from_causal_events_populates_lineage() -> anyhow::Result<()> {
    // Simulates a fork created BEFORE the lineage migration: the causal event
    // exists but no session_fork_lineage row was written. The migration v54
    // backfill harvests `session.forked` events to populate the table.
    let temp = tempdir()?;
    let store = GatewayStore::open(temp.path())?;

    let parent = "sess-backfill-parent";
    let fork = "fork-backfill";

    // Parent has an artifact ref.
    store.create_artifact_ref(&make_ref(
        "ar.bf1",
        ArtifactRefScopeType::Session,
        parent,
        "art_bf",
    ))?;

    // Simulate the causal event that the old fork handler logged.
    use autonoetic_types::causal_chain::CausalEventRecord;
    store.create_causal_event(&CausalEventRecord {
        event_id: "evt-backfill-test".to_string(),
        agent_id: "planner.default".to_string(),
        session_id: fork.to_string(),
        turn_id: None,
        event_seq: 1,
        timestamp: chrono::Utc::now().to_rfc3339(),
        category: "session".to_string(),
        action: "session.forked".to_string(),
        status: "success".to_string(),
        enforced_rules: vec!["R+++3".to_string()],
        target: None,
        payload: Some(serde_json::json!({"source_session_id": parent}).to_string()),
        payload_ref: None,
        evidence_ref: None,
        reason: None,
    })?;

    // Before backfill: fork can't resolve parent ref.
    let unresolved = store.resolve_artifact_ref_any_scope("ar.bf1", fork)?;
    assert!(unresolved.is_none(), "should not resolve before backfill");

    // Run the backfill (same SQL as migration v54).
    let n = store.backfill_fork_lineage_from_causal_events()?;
    assert_eq!(n, 1, "should backfill one lineage row");

    // After backfill: fork resolves parent ref.
    let resolved = store
        .resolve_artifact_ref_any_scope("ar.bf1", fork)?
        .expect("should resolve after backfill");
    assert_eq!(resolved.artifact_id, "art_bf");

    Ok(())
}

#[test]
fn forked_session_resolves_parent_workflow_scoped_ref() -> anyhow::Result<()> {
    let temp = tempdir()?;
    let store = GatewayStore::open(temp.path())?;

    // Parent session is part of a workflow; artifact is workflow-scoped.
    let parent = "session-parent";
    let wf_id = "wf-test123";

    // Register the parent root in the workflow index.
    store.set_workflow_index(parent, wf_id)?;

    store.create_artifact_ref(&make_ref(
        "ar.wf1",
        ArtifactRefScopeType::Workflow,
        wf_id,
        "art_wf",
    ))?;

    // Fork from the parent.
    let fork = "fork-wf-test";
    store.record_fork_lineage(fork, parent, 0, None, "test-agent")?;

    // The fork resolves the parent's workflow-scoped ref via lineage.
    let resolved = store
        .resolve_artifact_ref_any_scope("ar.wf1", fork)?
        .expect("fork should resolve parent's workflow-scoped ref");
    assert_eq!(resolved.artifact_id, "art_wf");
    assert_eq!(resolved.scope_type, ArtifactRefScopeType::Workflow);
    assert_eq!(resolved.scope_id, wf_id);

    Ok(())
}

#[test]
fn find_active_ref_walks_fork_lineage_for_workflow_scope() -> anyhow::Result<()> {
    let temp = tempdir()?;
    let store = GatewayStore::open(temp.path())?;

    let parent = "session-find-wf";
    let wf_id = "wf-find456";

    store.set_workflow_index(parent, wf_id)?;

    store.create_artifact_ref(&make_ref(
        "ar.findwf",
        ArtifactRefScopeType::Workflow,
        wf_id,
        "art_findwf",
    ))?;

    let fork = "fork-find-wf";
    store.record_fork_lineage(fork, parent, 0, None, "test-agent")?;

    let found = store
        .find_active_ref_for_artifact("art_findwf", fork)?
        .expect("should find workflow-scoped ref via fork lineage");
    assert_eq!(found, "ar.findwf");

    Ok(())
}
