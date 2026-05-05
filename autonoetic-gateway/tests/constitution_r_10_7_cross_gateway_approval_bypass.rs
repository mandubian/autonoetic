//! Constitution R-10.7 — approval grants cannot be reused across root sessions.

use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_types::background::{GrantScope, GrantTarget};
use tempfile::tempdir;

#[test]
fn r_10_7_cross_root_grant_reuse_is_rejected() -> anyhow::Result<()> {
    let temp = tempdir()?;
    let gateway_dir = temp.path().join(".gateway");
    std::fs::create_dir_all(&gateway_dir)?;
    let store = GatewayStore::open(&gateway_dir)?;

    let host = "api.example.com".to_string();
    store.insert_session_grant(
        "root-a",
        "root-a",
        "agent-a",
        &GrantScope::RootSession,
        &[GrantTarget::ExactHost(host.clone())],
        "operator",
        &chrono::Utc::now().to_rfc3339(),
        Some("apr-r-10-7"),
        None,
    )?;

    assert!(
        store.grants_cover_targets("root-a/child", "root-a", std::slice::from_ref(&host)),
        "grant should cover descendants in the original root session"
    );
    assert!(
        !store.grants_cover_targets("root-b/child", "root-b", std::slice::from_ref(&host)),
        "grant from root-a must not bypass approvals in a different root session"
    );

    Ok(())
}
