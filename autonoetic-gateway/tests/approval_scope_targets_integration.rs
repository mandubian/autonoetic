//! Integration tests for Phase 2 approval hardening: grant scope, targets, expiry.
//!
//! Verifies:
//!   1. Session-scoped grants only cover the specific child session.
//!   2. Root-scoped grants cover all children.
//!   3. GrantTarget pattern matching works for all four kinds.
//!   4. Expired grants are excluded from coverage.
//!   5. Emergency stop cleans up all scopes.
//!   6. Janitor prunes expired grants.

use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_types::background::{GrantScope, GrantTarget};

fn make_gateway_dir(tmp: &tempfile::TempDir) -> std::path::PathBuf {
    let gw = tmp.path().join(".gateway");
    std::fs::create_dir_all(&gw).unwrap();
    gw
}

fn seed_grant(
    store: &GatewayStore,
    root_sid: &str,
    session_id: &str,
    agent_id: &str,
    scope: &GrantScope,
    targets: &[GrantTarget],
    expires_at: Option<&str>,
) {
    store
        .insert_session_grant(
            root_sid,
            session_id,
            agent_id,
            scope,
            targets,
            "test",
            &chrono::Utc::now().to_rfc3339(),
            None,
            expires_at,
        )
        .unwrap();
}

#[test]
#[serial_test::serial]
fn test_session_scoped_grant_covers_only_own_session() {
    let tmp = tempfile::tempdir().unwrap();
    let gw = make_gateway_dir(&tmp);
    let store = GatewayStore::open(&gw).unwrap();

    let targets = vec![GrantTarget::ExactHost("api.example.com".to_string())];
    seed_grant(
        &store,
        "root-1",
        "child-A",
        "agent-1",
        &GrantScope::Session,
        &targets,
        None,
    );

    assert!(store.grants_cover_targets("child-A", "root-1", &["api.example.com".to_string()]));
    assert!(!store.grants_cover_targets("child-B", "root-1", &["api.example.com".to_string()]));
}

#[test]
#[serial_test::serial]
fn test_root_scoped_grant_covers_all_children() {
    let tmp = tempfile::tempdir().unwrap();
    let gw = make_gateway_dir(&tmp);
    let store = GatewayStore::open(&gw).unwrap();

    let targets = vec![GrantTarget::ExactHost("api.example.com".to_string())];
    seed_grant(
        &store,
        "root-1",
        "child-A",
        "agent-1",
        &GrantScope::RootSession,
        &targets,
        None,
    );

    assert!(store.grants_cover_targets("child-A", "root-1", &["api.example.com".to_string()]));
    assert!(store.grants_cover_targets("child-B", "root-1", &["api.example.com".to_string()]));
    assert!(store.grants_cover_targets("root-1", "root-1", &["api.example.com".to_string()]));
}

#[test]
#[serial_test::serial]
fn test_host_suffix_matches_subdomain() {
    let tmp = tempfile::tempdir().unwrap();
    let gw = make_gateway_dir(&tmp);
    let store = GatewayStore::open(&gw).unwrap();

    let targets = vec![GrantTarget::HostSuffix("*.github.com".to_string())];
    seed_grant(
        &store,
        "root-1",
        "root-1",
        "agent-1",
        &GrantScope::RootSession,
        &targets,
        None,
    );

    assert!(store.grants_cover_targets("root-1", "root-1", &["api.github.com".to_string()]));
    assert!(store.grants_cover_targets("root-1", "root-1", &["v2.api.github.com".to_string()]));
    assert!(!store.grants_cover_targets("root-1", "root-1", &["github.com.evil.example".to_string()]));
}

#[test]
#[serial_test::serial]
fn test_host_and_port_target() {
    let tmp = tempfile::tempdir().unwrap();
    let gw = make_gateway_dir(&tmp);
    let store = GatewayStore::open(&gw).unwrap();

    let targets = vec![GrantTarget::HostAndPort {
        host: "api.example.com".to_string(),
        port: 443,
    }];
    seed_grant(
        &store,
        "root-1",
        "root-1",
        "agent-1",
        &GrantScope::RootSession,
        &targets,
        None,
    );

    assert!(store.grants_cover_targets("root-1", "root-1", &["api.example.com:443".to_string()]));
    assert!(!store.grants_cover_targets("root-1", "root-1", &["api.example.com".to_string()]));
    assert!(!store.grants_cover_targets("root-1", "root-1", &["api.example.com:8080".to_string()]));
}

#[test]
#[serial_test::serial]
fn test_url_prefix_target() {
    let tmp = tempfile::tempdir().unwrap();
    let gw = make_gateway_dir(&tmp);
    let store = GatewayStore::open(&gw).unwrap();

    let targets = vec![GrantTarget::UrlPrefix("https://api.example.com/public/".to_string())];
    seed_grant(
        &store,
        "root-1",
        "root-1",
        "agent-1",
        &GrantScope::RootSession,
        &targets,
        None,
    );

    assert!(store.grants_cover_targets("root-1", "root-1", &["https://api.example.com/public/x".to_string()]));
    assert!(!store.grants_cover_targets("root-1", "root-1", &["https://api.example.com/admin".to_string()]));
}

#[test]
#[serial_test::serial]
fn test_expired_grant_excluded() {
    let tmp = tempfile::tempdir().unwrap();
    let gw = make_gateway_dir(&tmp);
    let store = GatewayStore::open(&gw).unwrap();

    let past = "2000-01-01T00:00:00+00:00";
    let targets = vec![GrantTarget::ExactHost("api.example.com".to_string())];
    seed_grant(
        &store,
        "root-1",
        "root-1",
        "agent-1",
        &GrantScope::RootSession,
        &targets,
        Some(past),
    );

    assert!(!store.grants_cover_targets("root-1", "root-1", &["api.example.com".to_string()]));
}

#[test]
#[serial_test::serial]
fn test_future_expiry_still_covers() {
    let tmp = tempfile::tempdir().unwrap();
    let gw = make_gateway_dir(&tmp);
    let store = GatewayStore::open(&gw).unwrap();

    let future = "2099-01-01T00:00:00+00:00";
    let targets = vec![GrantTarget::ExactHost("api.example.com".to_string())];
    seed_grant(
        &store,
        "root-1",
        "root-1",
        "agent-1",
        &GrantScope::RootSession,
        &targets,
        Some(future),
    );

    assert!(store.grants_cover_targets("root-1", "root-1", &["api.example.com".to_string()]));
}

#[test]
#[serial_test::serial]
fn test_prune_expired_grants() {
    let tmp = tempfile::tempdir().unwrap();
    let gw = make_gateway_dir(&tmp);
    let store = GatewayStore::open(&gw).unwrap();

    let past = "2000-01-01T00:00:00+00:00";
    let targets = vec![GrantTarget::ExactHost("expired.example.com".to_string())];
    seed_grant(
        &store,
        "root-1",
        "root-1",
        "agent-1",
        &GrantScope::RootSession,
        &targets,
        Some(past),
    );

    let targets2 = vec![GrantTarget::ExactHost("active.example.com".to_string())];
    let future = "2099-01-01T00:00:00+00:00";
    seed_grant(
        &store,
        "root-1",
        "root-1",
        "agent-1",
        &GrantScope::RootSession,
        &targets2,
        Some(future),
    );

    let pruned = store.prune_expired_grants().unwrap();
    assert_eq!(pruned, 1);

    assert!(!store.grants_cover_targets("root-1", "root-1", &["expired.example.com".to_string()]));
    assert!(store.grants_cover_targets("root-1", "root-1", &["active.example.com".to_string()]));
}

#[test]
#[serial_test::serial]
fn test_emergency_stop_cleans_up_all_scopes() {
    let tmp = tempfile::tempdir().unwrap();
    let gw = make_gateway_dir(&tmp);
    let store = GatewayStore::open(&gw).unwrap();

    let targets = vec![GrantTarget::ExactHost("api.example.com".to_string())];
    seed_grant(&store, "root-1", "child-A", "agent-1", &GrantScope::Session, &targets, None);
    seed_grant(&store, "root-1", "root-1", "agent-1", &GrantScope::RootSession, &targets, None);

    store.delete_session_grants("root-1").unwrap();

    assert!(!store.grants_cover_targets("child-A", "root-1", &["api.example.com".to_string()]));
    assert!(!store.grants_cover_targets("root-1", "root-1", &["api.example.com".to_string()]));
}

#[test]
#[serial_test::serial]
fn test_two_children_one_root_session_vs_root_scope() {
    let tmp = tempfile::tempdir().unwrap();
    let gw = make_gateway_dir(&tmp);
    let store = GatewayStore::open(&gw).unwrap();

    let targets_a = vec![GrantTarget::ExactHost("host-a.example.com".to_string())];
    seed_grant(&store, "root-1", "child-A", "agent-1", &GrantScope::Session, &targets_a, None);

    let targets_b = vec![GrantTarget::ExactHost("host-b.example.com".to_string())];
    seed_grant(&store, "root-1", "child-B", "agent-1", &GrantScope::Session, &targets_b, None);

    assert!(store.grants_cover_targets("child-A", "root-1", &["host-a.example.com".to_string()]));
    assert!(!store.grants_cover_targets("child-A", "root-1", &["host-b.example.com".to_string()]));
    assert!(store.grants_cover_targets("child-B", "root-1", &["host-b.example.com".to_string()]));
    assert!(!store.grants_cover_targets("child-B", "root-1", &["host-a.example.com".to_string()]));
}

#[test]
#[serial_test::serial]
fn test_multi_target_grant() {
    let tmp = tempfile::tempdir().unwrap();
    let gw = make_gateway_dir(&tmp);
    let store = GatewayStore::open(&gw).unwrap();

    let targets = vec![
        GrantTarget::ExactHost("api.example.com".to_string()),
        GrantTarget::ExactHost("cdn.example.com".to_string()),
    ];
    seed_grant(&store, "root-1", "root-1", "agent-1", &GrantScope::RootSession, &targets, None);

    assert!(store.grants_cover_targets("root-1", "root-1", &[
        "api.example.com".to_string(),
        "cdn.example.com".to_string(),
    ]));
    assert!(!store.grants_cover_targets("root-1", "root-1", &[
        "api.example.com".to_string(),
        "other.example.com".to_string(),
    ]));
}

#[test]
fn test_grant_target_exact_host_matches() {
    assert!(GrantTarget::ExactHost("api.github.com".to_string()).matches("api.github.com"));
    assert!(!GrantTarget::ExactHost("api.github.com".to_string()).matches("other.com"));
}

#[test]
fn test_grant_target_host_suffix_rejects_evil() {
    let t = GrantTarget::HostSuffix("*.github.com".to_string());
    assert!(t.matches("api.github.com"));
    assert!(t.matches("v2.api.github.com"));
    assert!(t.matches("github.com"));
    assert!(!t.matches("github.com.evil.example"));
}

#[test]
fn test_grant_target_host_and_port() {
    let t = GrantTarget::HostAndPort {
        host: "api.example.com".to_string(),
        port: 443,
    };
    assert!(t.matches("api.example.com:443"));
    assert!(!t.matches("api.example.com:80"));
    assert!(!t.matches("api.example.com"));
}

#[test]
fn test_grant_target_url_prefix() {
    let t = GrantTarget::UrlPrefix("https://api.example.com/public/".to_string());
    assert!(t.matches("https://api.example.com/public/x"));
    assert!(t.matches("https://api.example.com/public/"));
    assert!(!t.matches("https://api.example.com/admin"));
}
