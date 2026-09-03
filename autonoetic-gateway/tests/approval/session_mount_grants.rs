//! Session mount grants end-to-end (#1002 slice 5, issue #1296): an approved
//! sandbox exec whose manifest declared uncovered host mounts materializes
//! session-scoped mount grants, and the declared-mount resolver cures the
//! matching denial on the agent's retry.

use std::path::PathBuf;
use std::sync::Arc;

use autonoetic_gateway::sandbox::resolve_declared_mounts;
use autonoetic_gateway::scheduler::approval::{apply_decision, ApproveOptions, DecisionContext};
use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_types::agent::DeclaredMount;
use autonoetic_types::background::{
    ApprovalDecision, ApprovalLevel, ApprovalStatus, GrantScope, MountRequest, ScheduledAction,
};
use autonoetic_types::config::GatewayConfig;

fn decision_with_mounts(
    request_id: &str,
    session_id: &str,
    root: &str,
    mounts: Vec<MountRequest>,
) -> ApprovalDecision {
    ApprovalDecision {
        request_id: request_id.to_string(),
        session_id: session_id.to_string(),
        root_session_id: Some(root.to_string()),
        agent_id: "mount.tester".to_string(),
        action: ScheduledAction::SandboxExec {
            command: "cat /data/mail/notes".to_string(),
            dependencies: None,
            requires_approval: true,
            evidence_ref: None,
            detected_hosts: None,
            detected_mounts: Some(mounts),
            intent: None,
        },
        status: ApprovalStatus::Approved,
        decided_by: "operator".to_string(),
        decided_at: chrono::Utc::now().to_rfc3339(),
        reason: Some("expected mount for this task".to_string()),
        workflow_id: None,
        task_id: None,
        approval_level: ApprovalLevel::Operator,
    }
}

fn mount(host: &std::path::Path, readonly: bool) -> MountRequest {
    MountRequest {
        host_path: host.to_string_lossy().to_string(),
        canonical_path: host.canonicalize().unwrap().to_string_lossy().to_string(),
        readonly,
    }
}

fn declared(path: &std::path::Path, readonly: bool) -> DeclaredMount {
    DeclaredMount {
        host_path: path.to_string_lossy().to_string(),
        readonly,
    }
}

/// Raw row count for audit-trail assertions, read straight from the SQLite
/// file (the store's `conn` is private to the crate).
fn raw_row_count(gateway_dir: &std::path::Path, root: &str) -> i64 {
    let conn = rusqlite::Connection::open(gateway_dir.join("gateway.db")).unwrap();
    conn.query_row(
        "SELECT COUNT(*) FROM session_mount_grants WHERE root_session_id = ?1",
        [root],
        |r| r.get(0),
    )
    .unwrap()
}

/// The full slice-5 cycle: approval decision → grant row → resolver cure.
#[test]
fn approved_mount_request_materializes_grant_and_cures_denial() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let store = Arc::new(GatewayStore::open(tmp.path())?);
    let config = GatewayConfig::default();
    let outside = tmp.path().join("outside");
    std::fs::create_dir_all(&outside)?;

    // Before approval: the declaration is denied.
    let (granted, denied) = resolve_declared_mounts(
        &[declared(&outside, true)],
        &[],
        &[],
        &[],
        &store.active_session_mount_grants("root-mnt", "root-mnt", "mount.tester")?,
    );
    assert!(granted.is_empty());
    assert_eq!(denied.len(), 1, "pre-approval denial expected: {denied:?}");

    // Operator approves the request carrying the mount.
    apply_decision(
        &config,
        Some(&store),
        &decision_with_mounts(
            "apr-mnt-cure",
            "root-mnt",
            "root-mnt",
            vec![mount(&outside, true)],
        ),
        &ApproveOptions::default(),
        &DecisionContext {
            wiki_materialized_meta: None,
            hook_executor: None,
        },
    )?;

    // Grant row exists and is visible to the requesting agent/session...
    let grants = store.active_session_mount_grants("root-mnt", "root-mnt", "mount.tester")?;
    assert_eq!(grants.len(), 1, "one mount grant expected");
    assert_eq!(
        grants[0].canonical_path,
        outside.canonicalize()?.to_string_lossy().to_string()
    );
    assert!(grants[0].readonly);
    assert_eq!(grants[0].source_approval_id.as_deref(), Some("apr-mnt-cure"));

    // ...and the resolver now cures the denial.
    let (granted, denied) = resolve_declared_mounts(
        &[declared(&outside, true)],
        &[],
        &[],
        &[],
        &store.active_session_mount_grants("root-mnt", "root-mnt", "mount.tester")?,
    );
    assert!(denied.is_empty(), "post-approval denial must be cured: {denied:?}");
    assert_eq!(granted.len(), 1);
    assert_eq!(granted[0].source, outside.canonicalize()?);
    Ok(())
}

/// An rw request approved as rw materializes an rw grant, which cures both rw
/// and ro declarations under its prefix — but an ro grant never cures rw.
#[test]
fn rw_approval_grant_ceiling_holds() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let store = Arc::new(GatewayStore::open(tmp.path())?);
    let config = GatewayConfig::default();
    let rw_dir = tmp.path().join("rw-dir");
    std::fs::create_dir_all(&rw_dir)?;

    apply_decision(
        &config,
        Some(&store),
        &decision_with_mounts("apr-mnt-rw", "root-rw", "root-rw", vec![mount(&rw_dir, false)]),
        &ApproveOptions::default(),
        &DecisionContext {
            wiki_materialized_meta: None,
            hook_executor: None,
        },
    )?;
    let grants = store.active_session_mount_grants("root-rw", "root-rw", "mount.tester")?;
    assert_eq!(grants.len(), 1);
    assert!(!grants[0].readonly, "rw request must mint an rw grant");

    let (granted, denied) = resolve_declared_mounts(
        &[declared(&rw_dir, false), declared(&rw_dir, true)],
        &[],
        &[],
        &[],
        &grants,
    );
    assert!(denied.is_empty(), "rw grant cures both modes: {denied:?}");
    assert_eq!(granted.len(), 2);
    assert!(!granted[0].readonly && granted[1].readonly);
    Ok(())
}

/// `create_grant: false` approves the invocation without pre-authorizing the
/// paths — the same one-shot opt-out as the host grants.
#[test]
fn create_grant_false_mints_no_mount_grants() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let store = Arc::new(GatewayStore::open(tmp.path())?);
    let config = GatewayConfig::default();
    let dir = tmp.path().join("once");
    std::fs::create_dir_all(&dir)?;

    let mut options = ApproveOptions::default();
    options.create_grant = Some(false);
    apply_decision(
        &config,
        Some(&store),
        &decision_with_mounts("apr-mnt-once", "root-once", "root-once", vec![mount(&dir, true)]),
        &options,
        &DecisionContext {
            wiki_materialized_meta: None,
            hook_executor: None,
        },
    )?;
    assert!(store
        .active_session_mount_grants("root-once", "root-once", "mount.tester")?
        .is_empty());
    Ok(())
}

/// Session-scoped grants cover only their own session — a sibling under the
/// same root does not inherit the mount.
#[test]
fn session_scoped_grant_does_not_cover_sibling() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let store = Arc::new(GatewayStore::open(tmp.path())?);
    let config = GatewayConfig::default();
    let dir = tmp.path().join("scoped");
    std::fs::create_dir_all(&dir)?;

    let mut options = ApproveOptions::default();
    options.grant_scope = Some(GrantScope::Session);
    apply_decision(
        &config,
        Some(&store),
        &decision_with_mounts("apr-mnt-scope", "root-scope/child", "root-scope", vec![mount(&dir, true)]),
        &options,
        &DecisionContext {
            wiki_materialized_meta: None,
            hook_executor: None,
        },
    )?;
    assert_eq!(
        store
            .active_session_mount_grants("root-scope", "root-scope/child", "mount.tester")?
            .len(),
        1
    );
    assert!(
        store
            .active_session_mount_grants("root-scope", "root-scope/sibling", "mount.tester")?
            .is_empty(),
        "sibling session must not inherit the session-scoped mount grant"
    );
    Ok(())
}

/// Deny beats grant at mint time: a MountRequest over a protected path is
/// never materialized, even though the operator approved the action.
/// (nextest runs each test in its own process, so seeding the global
/// deny-path registry here mirrors gateway startup without leaking.)
#[test]
fn protected_paths_are_never_granted() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let store = Arc::new(GatewayStore::open(tmp.path())?);
    let config = GatewayConfig::default();
    let protected_dir = tmp.path().join("operator-secret");
    std::fs::create_dir_all(&protected_dir)?;
    autonoetic_gateway::sandbox::init_sandbox_host_deny_paths(vec![protected_dir.clone()]);

    apply_decision(
        &config,
        Some(&store),
        &decision_with_mounts(
            "apr-mnt-prot",
            "root-prot",
            "root-prot",
            vec![MountRequest {
                host_path: protected_dir.to_string_lossy().to_string(),
                canonical_path: protected_dir.to_string_lossy().to_string(),
                readonly: true,
            }],
        ),
        &ApproveOptions::default(),
        &DecisionContext {
            wiki_materialized_meta: None,
            hook_executor: None,
        },
    )?;
    assert!(
        store
            .active_session_mount_grants("root-prot", "root-prot", "mount.tester")?
            .is_empty(),
        "a grant over a protected path must never be minted"
    );
    Ok(())
}

/// Operator revocation and TTL expiry both close coverage; revoking a path
/// kills grants at or above it.
#[test]
fn revocation_expiry_and_path_scoped_revoke() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let store = Arc::new(GatewayStore::open(tmp.path())?);
    let config = GatewayConfig::default();
    let data = tmp.path().join("data");
    let mail = data.join("mail");
    std::fs::create_dir_all(&mail)?;

    apply_decision(
        &config,
        Some(&store),
        &decision_with_mounts(
            "apr-mnt-rev",
            "root-rev",
            "root-rev",
            vec![mount(&data, true), mount(&mail, true)],
        ),
        &ApproveOptions::default(),
        &DecisionContext {
            wiki_materialized_meta: None,
            hook_executor: None,
        },
    )?;
    assert_eq!(
        store
            .active_session_mount_grants("root-rev", "root-rev", "mount.tester")?
            .len(),
        2
    );

    // Revoking the parent path kills both grants (at-or-above semantics).
    let revoked = store.revoke_session_mount_grants(
        "root-rev",
        Some(&mail.to_string_lossy()),
        "operator revoke",
    )?;
    assert_eq!(revoked, 2, "revoke at-or-above must kill both grants");
    assert!(store
        .active_session_mount_grants("root-rev", "root-rev", "mount.tester")?
        .is_empty());

    // TTL: a grant with a short expiry lapses out of the active set.
    apply_decision(
        &config,
        Some(&store),
        &decision_with_mounts(
            "apr-mnt-ttl",
            "root-ttl",
            "root-ttl",
            vec![mount(&data, true)],
        ),
        &ApproveOptions {
            grant_expires_at: Some("2026-01-01T00:00:00Z".to_string()),
            ..Default::default()
        },
        &DecisionContext {
            wiki_materialized_meta: None,
            hook_executor: None,
        },
    )?;
    let grants = store.active_session_mount_grants("root-ttl", "root-ttl", "mount.tester")?;
    assert!(
        grants.is_empty(),
        "lapsed grant must not surface: expires_at={:?}",
        grants.first().map(|g| &g.expires_at)
    );
    // The row survives as audit trail; the reaper is what deletes it.
    assert_eq!(
        raw_row_count(tmp.path(), "root-ttl"),
        1,
        "revocation/expiry keeps the row as audit trail"
    );

    // Emergency-stop cleanup hard-deletes.
    store.delete_session_mount_grants("root-ttl")?;
    assert_eq!(raw_row_count(tmp.path(), "root-ttl"), 0);
    Ok(())
}

/// The default TTL (`default_grant_ttl_secs`) stamps `expires_at` on the
/// grant, same as the host grants.
#[test]
fn default_ttl_stamps_expiry() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let store = Arc::new(GatewayStore::open(tmp.path())?);
    let mut config = GatewayConfig::default();
    config.default_grant_ttl_secs = 3600;
    let dir = tmp.path().join("ttl");
    std::fs::create_dir_all(&dir)?;

    apply_decision(
        &config,
        Some(&store),
        &decision_with_mounts("apr-mnt-ttl2", "root-ttl2", "root-ttl2", vec![mount(&dir, true)]),
        &ApproveOptions::default(),
        &DecisionContext {
            wiki_materialized_meta: None,
            hook_executor: None,
        },
    )?;
    let grants = store.active_session_mount_grants("root-ttl2", "root-ttl2", "mount.tester")?;
    assert_eq!(grants.len(), 1);
    let expires = grants[0].expires_at.as_deref().expect("expires_at stamped");
    let dt = chrono::DateTime::parse_from_rfc3339(expires)?;
    assert!(
        dt > chrono::Utc::now(),
        "expiry must be in the future ({expires})"
    );
    Ok(())
}
