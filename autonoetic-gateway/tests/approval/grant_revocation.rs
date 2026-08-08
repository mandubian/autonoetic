use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use tempfile::tempdir;

fn make_gateway_dir(tmp: &tempfile::TempDir) -> std::path::PathBuf {
    let gw = tmp.path().join(".gateway");
    std::fs::create_dir_all(&gw).unwrap();
    gw
}

fn seed_grant(store: &GatewayStore, root_sid: &str, agent_id: &str, host: &str) {
    store
        .insert_session_grant_hosts(
            root_sid,
            agent_id,
            &[host.to_string()],
            "test",
            &chrono::Utc::now().to_rfc3339(),
            None,
        )
        .unwrap();
}

#[test]
fn test_revoke_grant_by_host() {
    let tmp = tempdir().unwrap();
    let gw = make_gateway_dir(&tmp);
    let store = GatewayStore::open(&gw).unwrap();

    seed_grant(&store, "root-1", "agent-a", "pypi.org");
    seed_grant(&store, "root-1", "agent-a", "github.com");
    seed_grant(&store, "root-1", "agent-a", "crates.io");

    let count = store
        .revoke_session_grants("root-1", Some("pypi.org"), "compromised")
        .unwrap();
    assert_eq!(count, 1);

    let grants = store.get_session_grants("root-1").unwrap();
    assert_eq!(grants, vec!["crates.io", "github.com"]);

    assert!(!store.session_grants_cover_targets("root-1", "agent-a", &["pypi.org".to_string()]));
    assert!(store.session_grants_cover_targets("root-1", "agent-a", &["github.com".to_string()]));
}

#[test]
fn test_revoke_all_grants_for_session() {
    let tmp = tempdir().unwrap();
    let gw = make_gateway_dir(&tmp);
    let store = GatewayStore::open(&gw).unwrap();

    seed_grant(&store, "root-2", "agent-b", "host1.com");
    seed_grant(&store, "root-2", "agent-b", "host2.com");
    seed_grant(&store, "other-root", "agent-c", "host3.com");

    let count = store
        .revoke_session_grants("root-2", None, "credential rotation")
        .unwrap();
    assert_eq!(count, 2);

    assert!(store.get_session_grants("root-2").unwrap().is_empty());
    assert_eq!(
        store.get_session_grants("other-root").unwrap(),
        vec!["host3.com"]
    );
}

#[test]
fn test_revoke_idempotent_no_double_revoke() {
    let tmp = tempdir().unwrap();
    let gw = make_gateway_dir(&tmp);
    let store = GatewayStore::open(&gw).unwrap();

    seed_grant(&store, "root-3", "agent-d", "host.io");

    let count1 = store
        .revoke_session_grants("root-3", Some("host.io"), "first")
        .unwrap();
    assert_eq!(count1, 1);

    let count2 = store
        .revoke_session_grants("root-3", Some("host.io"), "second")
        .unwrap();
    assert_eq!(count2, 0);

    assert!(store.get_session_grants("root-3").unwrap().is_empty());
}

#[test]
fn test_grants_cover_targets_after_partial_revoke() {
    let tmp = tempdir().unwrap();
    let gw = make_gateway_dir(&tmp);
    let store = GatewayStore::open(&gw).unwrap();

    seed_grant(&store, "root-4", "agent-e", "a.com");
    seed_grant(&store, "root-4", "agent-e", "b.com");
    seed_grant(&store, "root-4", "agent-e", "c.com");

    assert!(
        store.session_grants_cover_targets("root-4", "agent-e", &["a.com".to_string(), "b.com".to_string()])
    );

    store
        .revoke_session_grants("root-4", Some("b.com"), "revoked")
        .unwrap();

    assert!(
        !store.session_grants_cover_targets("root-4", "agent-e", &["a.com".to_string(), "b.com".to_string()])
    );
    assert!(store.session_grants_cover_targets("root-4", "agent-e", &["a.com".to_string()]));
    assert!(store.session_grants_cover_targets("root-4", "agent-e", &["c.com".to_string()]));
}

#[test]
fn test_delete_grants_removes_revoked_and_active() {
    let tmp = tempdir().unwrap();
    let gw = make_gateway_dir(&tmp);
    let store = GatewayStore::open(&gw).unwrap();

    seed_grant(&store, "root-5", "agent-f", "x.io");
    store
        .revoke_session_grants("root-5", Some("x.io"), "before delete")
        .unwrap();

    store.delete_session_grants("root-5").unwrap();

    assert!(store.get_session_grants("root-5").unwrap().is_empty());
}

/// By-id revoke (the TUI grants panel's per-row path) must be scoped to the
/// root session that owns the grant. Row ids are `AUTOINCREMENT` and therefore
/// enumerable, so an id must never be usable as a capability: a caller naming
/// another root's id gets an idempotent no-op, and that root's grant keeps
/// covering its host. The owner can still revoke normally.
#[test]
fn test_revoke_grant_by_id_is_scoped_to_owning_root() {
    let tmp = tempdir().unwrap();
    let gw = make_gateway_dir(&tmp);
    let store = GatewayStore::open(&gw).unwrap();

    seed_grant(&store, "root-owner", "agent-h", "owned.io");
    seed_grant(&store, "root-stranger", "agent-i", "other.io");

    let gid = store
        .get_session_grants_structured("root-owner")
        .unwrap()
        .first()
        .expect("owner grant present")
        .id;

    // Stranger names the owner's id — no-op, and coverage is untouched.
    assert!(
        !store
            .revoke_session_grant_by_id("root-stranger", gid, "wrong root")
            .unwrap(),
        "a foreign root's by-id revoke must not land"
    );
    assert!(store.session_grants_cover_targets("root-owner", "agent-h", &["owned.io".to_string()]));
    assert_eq!(
        store.get_session_grants("root-owner").unwrap(),
        vec!["owned.io"]
    );

    // The owning root revokes it, and only it.
    assert!(store
        .revoke_session_grant_by_id("root-owner", gid, "operator: tui revoke")
        .unwrap());
    assert!(!store.session_grants_cover_targets("root-owner", "agent-h", &["owned.io".to_string()]));
    assert!(store.session_grants_cover_targets("root-stranger", "agent-i", &["other.io".to_string()]));

    // Second revoke by the owner is an idempotent no-op.
    assert!(!store
        .revoke_session_grant_by_id("root-owner", gid, "again")
        .unwrap());
}

#[test]
fn test_revoke_nonexistent_host_is_noop() {
    let tmp = tempdir().unwrap();
    let gw = make_gateway_dir(&tmp);
    let store = GatewayStore::open(&gw).unwrap();

    seed_grant(&store, "root-6", "agent-g", "exists.io");

    let count = store
        .revoke_session_grants("root-6", Some("nope.io"), "nothing")
        .unwrap();
    assert_eq!(count, 0);

    assert_eq!(
        store.get_session_grants("root-6").unwrap(),
        vec!["exists.io"]
    );
}

#[test]
fn test_revoke_then_reapprove_restores_coverage() {
    let tmp = tempdir().unwrap();
    let gw = make_gateway_dir(&tmp);
    let store = GatewayStore::open(&gw).unwrap();

    seed_grant(&store, "root-7", "agent-h", "regranted.io");

    assert!(store.session_grants_cover_targets("root-7", "agent-h", &["regranted.io".to_string()]));

    store
        .revoke_session_grants("root-7", Some("regranted.io"), "temp revoke")
        .unwrap();

    assert!(store.get_session_grants("root-7").unwrap().is_empty());
    assert!(!store.session_grants_cover_targets("root-7", "agent-h", &["regranted.io".to_string()]));

    seed_grant(&store, "root-7", "agent-h", "regranted.io");

    assert_eq!(
        store.get_session_grants("root-7").unwrap(),
        vec!["regranted.io"]
    );
    assert!(store.session_grants_cover_targets("root-7", "agent-h", &["regranted.io".to_string()]));
}

/// Pillar C: surgical revoke of every active grant whose
/// `source_approval_id` matches. Used when a plan's envelope expands and the
/// grants materialized from the prior approved revision must be withdrawn so
/// the new approval re-materializes a clean envelope. Grants from other
/// sources (explicit operator grants, other plans) must be untouched.
#[test]
fn test_revoke_session_grants_by_source_is_surgical() {
    let tmp = tempdir().unwrap();
    let gw = make_gateway_dir(&tmp);
    let store = GatewayStore::open(&gw).unwrap();
    let now = chrono::Utc::now().to_rfc3339();
    let scope = autonoetic_types::background::GrantScope::RootSession;

    // Plan A grants (source = plan-A) + a sibling plan B grant (source = plan-B)
    // + an explicit operator grant (source = None). The revoke of plan-A must
    // touch only plan-A's grants.
    for (agent, host) in [("a", "alpha.com"), ("a", "beta.com"), ("a", "gamma.com")] {
        store
            .insert_session_grant(
                "root-surg",
                "root-surg",
                agent,
                &scope,
                &[autonoetic_types::background::GrantTarget::ExactHost(
                    host.to_string(),
                )],
                "plan-A",
                &now,
                Some("plan-A"),
                None,
            )
            .unwrap();
    }
    store
        .insert_session_grant(
            "root-surg",
            "root-surg",
            "b",
            &scope,
            &[autonoetic_types::background::GrantTarget::ExactHost(
                "delta.com".to_string(),
            )],
            "plan-B",
            &now,
            Some("plan-B"),
            None,
        )
        .unwrap();
    store
        .insert_session_grant(
            "root-surg",
            "root-surg",
            "op",
            &scope,
            &[autonoetic_types::background::GrantTarget::ExactHost(
                "epsilon.com".to_string(),
            )],
            "operator",
            &now,
            None,
            None,
        )
        .unwrap();

    // Before revoke: coverage holds for each host under its owning agent.
    for h in ["alpha.com", "beta.com", "gamma.com"] {
        assert!(store.session_grants_cover_targets("root-surg", "a", &[h.to_string()]));
    }
    assert!(store.session_grants_cover_targets("root-surg", "b", &["delta.com".to_string()]));
    assert!(store.session_grants_cover_targets("root-surg", "op", &["epsilon.com".to_string()]));

    // Revoke plan-A's grants only.
    let n = store
        .revoke_session_grants_by_source("root-surg", "plan-A", "plan-amended")
        .unwrap();
    assert_eq!(n, 3, "exactly plan-A's 3 grants should be revoked");

    // Coverage: plan-A hosts no longer covered; plan-B + operator still covered.
    for h in ["alpha.com", "beta.com", "gamma.com"] {
        assert!(
            !store.session_grants_cover_targets("root-surg", "a", &[h.to_string()]),
            "plan-A host {h} should be uncovered after revoke"
        );
    }
    for (agent, h) in [("b", "delta.com"), ("op", "epsilon.com")] {
        assert!(
            store.session_grants_cover_targets("root-surg", agent, &[h.to_string()]),
            "non-plan-A host {h} must remain covered"
        );
    }

    // Revoking an unknown source is a no-op.
    let n = store
        .revoke_session_grants_by_source("root-surg", "plan-NONEXISTENT", "x")
        .unwrap();
    assert_eq!(n, 0);
}
