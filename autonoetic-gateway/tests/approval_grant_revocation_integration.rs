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

    assert!(!store.session_grants_cover_targets("root-1", &["pypi.org".to_string()]));
    assert!(store.session_grants_cover_targets("root-1", &["github.com".to_string()]));
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
    assert_eq!(store.get_session_grants("other-root").unwrap(), vec!["host3.com"]);
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

    assert!(store
        .session_grants_cover_targets("root-4", &["a.com".to_string(), "b.com".to_string()]));

    store
        .revoke_session_grants("root-4", Some("b.com"), "revoked")
        .unwrap();

    assert!(!store
        .session_grants_cover_targets("root-4", &["a.com".to_string(), "b.com".to_string()]));
    assert!(store.session_grants_cover_targets("root-4", &["a.com".to_string()]));
    assert!(store.session_grants_cover_targets("root-4", &["c.com".to_string()]));
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

    assert_eq!(store.get_session_grants("root-6").unwrap(), vec!["exists.io"]);
}

#[test]
fn test_revoke_then_reapprove_restores_coverage() {
    let tmp = tempdir().unwrap();
    let gw = make_gateway_dir(&tmp);
    let store = GatewayStore::open(&gw).unwrap();

    seed_grant(&store, "root-7", "agent-h", "regranted.io");

    assert!(store
        .session_grants_cover_targets("root-7", &["regranted.io".to_string()]));

    store
        .revoke_session_grants("root-7", Some("regranted.io"), "temp revoke")
        .unwrap();

    assert!(store.get_session_grants("root-7").unwrap().is_empty());
    assert!(!store
        .session_grants_cover_targets("root-7", &["regranted.io".to_string()]));

    seed_grant(&store, "root-7", "agent-h", "regranted.io");

    assert_eq!(store.get_session_grants("root-7").unwrap(), vec!["regranted.io"]);
    assert!(store
        .session_grants_cover_targets("root-7", &["regranted.io".to_string()]));
}
