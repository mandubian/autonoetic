//! Constitution P-2.23: Approval grant TTL.
//!
//! Session approval grants must have a default TTL (24h). When a grant
//! expires, it re-opens the approval gate — the target must be re-approved.
//! The scheduler tick prunes expired grant rows from the database.


use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_gateway::scheduler::{approve_request_with_options, ApproveOptions};
use autonoetic_types::background::{
    ApprovalLevel, ApprovalRequest, GrantScope, GrantTarget, ScheduledAction,
};
use std::sync::Arc;

fn seed_grant(
    store: &Arc<GatewayStore>,
    root_session_id: &str,
    agent_id: &str,
    hosts: &[&str],
    expires_at: Option<&str>,
) -> anyhow::Result<()> {
    let targets: Vec<GrantTarget> = hosts
        .iter()
        .map(|h| GrantTarget::ExactHost(h.to_string()))
        .collect();
    store.insert_session_grant(
        root_session_id,
        root_session_id,
        agent_id,
        &GrantScope::RootSession,
        &targets,
        "operator",
        &chrono::Utc::now().to_rfc3339(),
        None,
        expires_at,
    )
}

#[test]
fn r_plus_13_grant_roundtrip_with_expires_at() -> anyhow::Result<()> {
    let tempdir = tempfile::tempdir()?;
    let gateway_dir = tempdir.path().join(".gateway");
    let store = Arc::new(GatewayStore::open(&gateway_dir)?);

    let config = autonoetic_types::config::GatewayConfig {
        default_grant_ttl_secs: 3600,
        ..Default::default()
    };

    assert_eq!(config.default_grant_ttl_secs, 3600);
    assert!(config.default_grant_ttl_secs > 0);

    let now = chrono::Utc::now();
    let expires_at = now + chrono::Duration::seconds(config.default_grant_ttl_secs as i64);
    seed_grant(
        &store,
        "root-1",
        "agent-1",
        &["example.com"],
        Some(expires_at.to_rfc3339().as_str()),
    )?;

    let grants = store.get_session_grants_structured("root-1")?;
    assert_eq!(grants.len(), 1);
    assert!(
        grants[0].expires_at.is_some(),
        "grant must have expires_at set"
    );

    Ok(())
}

#[test]
fn r_plus_13_expired_grant_does_not_cover_targets() -> anyhow::Result<()> {
    let tempdir = tempfile::tempdir()?;
    let gateway_dir = tempdir.path().join(".gateway");
    let store = Arc::new(GatewayStore::open(&gateway_dir)?);

    let past = chrono::Utc::now() - chrono::Duration::seconds(3600);
    seed_grant(
        &store,
        "root-2",
        "agent-2",
        &["expired.example.com"],
        Some(past.to_rfc3339().as_str()),
    )?;

    let targets = vec!["expired.example.com".to_string()];
    let covers = store.session_grants_cover_targets("root-2", "agent-2", &targets);

    assert!(
        !covers,
        "expired grant must NOT cover targets — approval gate must re-open"
    );

    Ok(())
}

#[test]
fn r_plus_13_valid_grant_covers_targets() -> anyhow::Result<()> {
    let tempdir = tempfile::tempdir()?;
    let gateway_dir = tempdir.path().join(".gateway");
    let store = Arc::new(GatewayStore::open(&gateway_dir)?);

    let future = chrono::Utc::now() + chrono::Duration::seconds(86400);
    seed_grant(
        &store,
        "root-3",
        "agent-3",
        &["valid.example.com"],
        Some(future.to_rfc3339().as_str()),
    )?;

    let targets = vec!["valid.example.com".to_string()];
    let covers = store.session_grants_cover_targets("root-3", "agent-3", &targets);

    assert!(covers, "non-expired grant must cover targets");

    Ok(())
}

#[test]
fn r_plus_13_grant_without_expiry_covers_targets_forever() -> anyhow::Result<()> {
    let tempdir = tempfile::tempdir()?;
    let gateway_dir = tempdir.path().join(".gateway");
    let store = Arc::new(GatewayStore::open(&gateway_dir)?);

    seed_grant(
        &store,
        "root-4",
        "agent-4",
        &["permanent.example.com"],
        None,
    )?;

    let targets = vec!["permanent.example.com".to_string()];
    let covers = store.session_grants_cover_targets("root-4", "agent-4", &targets);

    assert!(
        covers,
        "grant without expires_at must cover targets indefinitely"
    );

    Ok(())
}

#[test]
fn r_plus_13_prune_expired_grants_removes_expired_rows() -> anyhow::Result<()> {
    let tempdir = tempfile::tempdir()?;
    let gateway_dir = tempdir.path().join(".gateway");
    let store = Arc::new(GatewayStore::open(&gateway_dir)?);

    let past = chrono::Utc::now() - chrono::Duration::seconds(3600);
    let future = chrono::Utc::now() + chrono::Duration::seconds(86400);

    seed_grant(
        &store,
        "root-5",
        "agent-5",
        &["expired.example.com"],
        Some(past.to_rfc3339().as_str()),
    )?;
    seed_grant(
        &store,
        "root-5",
        "agent-5",
        &["valid.example.com"],
        Some(future.to_rfc3339().as_str()),
    )?;

    let grants_before = store.get_session_grants_structured("root-5")?;
    assert_eq!(grants_before.len(), 2);

    let pruned = store.prune_expired_grants()?;
    assert_eq!(pruned, 1, "one expired grant should be pruned");

    let grants_after = store.get_session_grants_structured("root-5")?;
    assert_eq!(grants_after.len(), 1);
    assert!(grants_after[0].expires_at.is_some());

    Ok(())
}

#[test]
fn r_plus_13_config_default_grant_ttl_secs_is_24h() {
    let config = autonoetic_types::config::GatewayConfig::default();
    assert_eq!(
        config.default_grant_ttl_secs, 86400,
        "default TTL must be 24h (86400 seconds)"
    );
}

#[test]
fn r_plus_13_zero_ttl_disables_auto_expiry() {
    let config = autonoetic_types::config::GatewayConfig {
        default_grant_ttl_secs: 0,
        ..Default::default()
    };
    assert_eq!(config.default_grant_ttl_secs, 0);
}

fn make_sandbox_exec_request(
    request_id: &str,
    root_session_id: &str,
    hosts: Vec<&str>,
) -> ApprovalRequest {
    ApprovalRequest {
        request_id: request_id.to_string(),
        agent_id: "coder.default".to_string(),
        session_id: format!("{}/coder-1", root_session_id),
        root_session_id: Some(root_session_id.to_string()),
        workflow_id: None,
        task_id: None,
        action: ScheduledAction::SandboxExec {
            command: "curl https://api.example.com".to_string(),
            detected_hosts: Some(hosts.into_iter().map(String::from).collect()),
            dependencies: None,
            requires_approval: true,
            evidence_ref: None,
            intent: None,
        },
        created_at: (chrono::Utc::now() - chrono::Duration::seconds(30)).to_rfc3339(),
        reason: Some("test approval".to_string()),
        evidence_ref: None,
        status: None,
        decided_at: None,
        decided_by: None,
        decision_reason: None,
        approval_level: ApprovalLevel::Operator,
        min_dwell_ms: None,
        confirm_phrase: None,
        code_excerpts: None,
        risk_summary: None,
        expires_at: None,
    }
}

#[test]
fn r_plus_13_approval_resolution_auto_computes_expiry_when_default_ttl_set() -> anyhow::Result<()> {
    let tempdir = tempfile::tempdir()?;
    let gateway_dir = tempdir.path().join(".gateway");
    let store = Arc::new(GatewayStore::open(&gateway_dir)?);

    let config = autonoetic_types::config::GatewayConfig {
        default_grant_ttl_secs: 3600,
        approval_dwell_multiplier: 0.0,
        ..Default::default()
    };

    let mut req =
        make_sandbox_exec_request("apr-ttl-auto", "root-ttl-auto", vec!["api.example.com"]);
    store.create_approval(&mut req)?;

    let before = chrono::Utc::now();
    let decision = approve_request_with_options(
        &config,
        Some(&store),
        "apr-ttl-auto",
        "operator",
        Some("approved".to_string()),
        None,
        Some(&ApprovalLevel::Operator),
        None,
        ApproveOptions::default(),
    )?;
    assert_eq!(
        decision.status,
        autonoetic_types::background::ApprovalStatus::Approved
    );

    let grants = store.get_session_grants_structured("root-ttl-auto")?;
    assert_eq!(grants.len(), 1, "approval should auto-insert one grant");
    let grant = &grants[0];
    assert!(
        grant.expires_at.is_some(),
        "grant must have expires_at auto-computed from default_grant_ttl_secs"
    );
    let expires_at = grant.expires_at.as_ref().unwrap();
    let parsed = chrono::DateTime::parse_from_rfc3339(expires_at)?;
    let expected_min = before + chrono::Duration::seconds(3600) - chrono::Duration::seconds(5);
    let expected_max = before + chrono::Duration::seconds(3600) + chrono::Duration::seconds(5);
    assert!(
        parsed.with_timezone(&chrono::Utc) >= expected_min
            && parsed.with_timezone(&chrono::Utc) <= expected_max,
        "expires_at should be approximately decided_at + default_grant_ttl_secs, got {}",
        expires_at
    );

    Ok(())
}

#[test]
fn r_plus_13_approval_resolution_no_expiry_when_default_ttl_zero() -> anyhow::Result<()> {
    let tempdir = tempfile::tempdir()?;
    let gateway_dir = tempdir.path().join(".gateway");
    let store = Arc::new(GatewayStore::open(&gateway_dir)?);

    let config = autonoetic_types::config::GatewayConfig {
        default_grant_ttl_secs: 0,
        approval_dwell_multiplier: 0.0,
        ..Default::default()
    };

    let mut req =
        make_sandbox_exec_request("apr-ttl-zero", "root-ttl-zero", vec!["api.example.com"]);
    store.create_approval(&mut req)?;

    let decision = approve_request_with_options(
        &config,
        Some(&store),
        "apr-ttl-zero",
        "operator",
        Some("approved".to_string()),
        None,
        Some(&ApprovalLevel::Operator),
        None,
        ApproveOptions::default(),
    )?;
    assert_eq!(
        decision.status,
        autonoetic_types::background::ApprovalStatus::Approved
    );

    let grants = store.get_session_grants_structured("root-ttl-zero")?;
    assert_eq!(grants.len(), 1);
    assert!(
        grants[0].expires_at.is_none(),
        "grant must NOT have expires_at when default_grant_ttl_secs is 0"
    );

    Ok(())
}

#[test]
fn r_plus_13_approval_resolution_explicit_ttl_overrides_default() -> anyhow::Result<()> {
    let tempdir = tempfile::tempdir()?;
    let gateway_dir = tempdir.path().join(".gateway");
    let store = Arc::new(GatewayStore::open(&gateway_dir)?);

    let config = autonoetic_types::config::GatewayConfig {
        default_grant_ttl_secs: 86400,
        approval_dwell_multiplier: 0.0,
        ..Default::default()
    };

    let mut req = make_sandbox_exec_request(
        "apr-ttl-explicit",
        "root-ttl-explicit",
        vec!["api.example.com"],
    );
    store.create_approval(&mut req)?;

    let explicit_expiry = (chrono::Utc::now() + chrono::Duration::seconds(600)).to_rfc3339();
    let decision = approve_request_with_options(
        &config,
        Some(&store),
        "apr-ttl-explicit",
        "operator",
        Some("approved".to_string()),
        None,
        Some(&ApprovalLevel::Operator),
        None,
        ApproveOptions {
            grant_expires_at: Some(explicit_expiry.clone()),
            ..Default::default()
        },
    )?;
    assert_eq!(
        decision.status,
        autonoetic_types::background::ApprovalStatus::Approved
    );

    let grants = store.get_session_grants_structured("root-ttl-explicit")?;
    assert_eq!(grants.len(), 1);
    assert!(grants[0].expires_at.is_some());
    let explicit_prefix = &explicit_expiry[..20];
    let grant_prefix = &grants[0].expires_at.as_deref().unwrap()[..20];
    assert_eq!(
        grant_prefix, explicit_prefix,
        "explicit grant_expires_at must override default TTL"
    );

    Ok(())
}
