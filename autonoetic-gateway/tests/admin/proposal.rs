//! Integration tests for admin proposal CRUD and tools.
//!
//! Run with:
//!   cargo test -p autonoetic-gateway --test admin_proposal_integration -- --nocapture


use autonoetic_gateway::policy::PolicyEngine;
use autonoetic_gateway::runtime::tools::default_registry;
use autonoetic_gateway::scheduler::gateway_store::admin_proposals::AdminProposal;
use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_types::agent::{AgentIdentity, AgentManifest};
use autonoetic_types::capability::Capability;
use serde_json::json;
use std::sync::Arc;
use tempfile::tempdir;
use crate::support::manifest_builder::TestManifest;

fn test_manifest_with_approval_queue() -> AgentManifest {
    AgentManifest {
        agent: AgentIdentity {
            id: "evolution-orchestrator.default".to_string(),
            name: "Evolution Orchestrator".to_string(),
            description: "test".to_string(),
            singleton: false,
            resident_idle_ttl_secs: None,
        },
        capabilities: vec![Capability::ApprovalQueue {
            patterns: vec!["admin.proposal.*".to_string()],
        }],
        ..TestManifest::new().build()
    }
}

fn test_manifest_with_read_access() -> AgentManifest {
    AgentManifest {
        agent: AgentIdentity {
            id: "admin-agent".to_string(),
            name: "Admin Agent".to_string(),
            description: "test".to_string(),
            singleton: false,
            resident_idle_ttl_secs: None,
        },
        capabilities: vec![Capability::ReadAccess {
            scopes: vec!["*".to_string()],
        }],
        ..TestManifest::new().build()
    }
}

fn test_manifest_no_caps() -> AgentManifest {
    AgentManifest {
        agent: AgentIdentity {
            id: "bare-agent".to_string(),
            name: "Bare Agent".to_string(),
            description: "test".to_string(),
            singleton: false,
            resident_idle_ttl_secs: None,
        },
        ..TestManifest::new().build()
    }
}

fn make_proposal(id: &str, title: &str, category: &str) -> AdminProposal {
    AdminProposal {
        proposal_id: id.to_string(),
        title: title.to_string(),
        category: category.to_string(),
        evidence_json: json!({"sessions": ["s1", "s2"], "pattern": "timeout"}),
        remediation: "Add retry logic".to_string(),
        blast_radius: "medium".to_string(),
        priority: "high".to_string(),
        created_by: "memory-curator.default".to_string(),
        created_at: chrono::Utc::now().to_rfc3339(),
        status: "open".to_string(),
        triaged_by: None,
        triaged_at: None,
        decision_reason: None,
    }
}

#[test]
fn test_admin_proposal_crud() -> anyhow::Result<()> {
    let temp_dir = tempdir()?;
    let store = GatewayStore::open(temp_dir.path())?;

    let p = make_proposal("prop-test001", "Missing HTTP retry tool", "tool");
    store.insert_admin_proposal(&p)?;

    let fetched = store.get_admin_proposal("prop-test001")?;
    assert!(fetched.is_some());
    let f = fetched.unwrap();
    assert_eq!(f.title, "Missing HTTP retry tool");
    assert_eq!(f.category, "tool");
    assert_eq!(f.status, "open");
    assert!(f.triaged_by.is_none());

    let p2 = make_proposal(
        "prop-test002",
        "Agents lack structured output parser",
        "capability",
    );
    store.insert_admin_proposal(&p2)?;

    let all = store.list_admin_proposals(None, None, 100)?;
    assert_eq!(all.len(), 2);

    let tools_only = store.list_admin_proposals(None, Some("tool"), 100)?;
    assert_eq!(tools_only.len(), 1);
    assert_eq!(tools_only[0].proposal_id, "prop-test001");

    Ok(())
}

#[test]
fn test_admin_proposal_status_update() -> anyhow::Result<()> {
    let temp_dir = tempdir()?;
    let store = GatewayStore::open(temp_dir.path())?;

    let p = make_proposal("prop-status001", "Test proposal", "protocol");
    store.insert_admin_proposal(&p)?;

    let updated = store.update_admin_proposal_status(
        "prop-status001",
        "accepted",
        "admin-operator",
        Some("Valid gap, scheduling implementation"),
    )?;
    assert!(updated);

    let fetched = store.get_admin_proposal("prop-status001")?.unwrap();
    assert_eq!(fetched.status, "accepted");
    assert_eq!(fetched.triaged_by.unwrap(), "admin-operator");
    assert!(fetched.triaged_at.is_some());
    assert_eq!(
        fetched.decision_reason.unwrap(),
        "Valid gap, scheduling implementation"
    );

    let missing = store.update_admin_proposal_status("nonexistent", "rejected", "admin", None)?;
    assert!(!missing);

    Ok(())
}

#[test]
fn test_admin_proposal_dedup() -> anyhow::Result<()> {
    let temp_dir = tempdir()?;
    let store = GatewayStore::open(temp_dir.path())?;

    let p = make_proposal("prop-dedup001", "Network timeout pattern", "protocol");
    store.insert_admin_proposal(&p)?;

    let matches =
        store.find_open_proposals_by_title_category("Network timeout pattern", "protocol")?;
    assert_eq!(matches.len(), 1);
    assert_eq!(matches[0].proposal_id, "prop-dedup001");

    let no_match =
        store.find_open_proposals_by_title_category("Completely different", "protocol")?;
    assert!(no_match.is_empty());

    Ok(())
}

#[test]
fn test_admin_proposal_list_by_status() -> anyhow::Result<()> {
    let temp_dir = tempdir()?;
    let store = GatewayStore::open(temp_dir.path())?;

    for i in 0..5 {
        let p = make_proposal(&format!("prop-ls-{i:03}"), &format!("Proposal {i}"), "tool");
        store.insert_admin_proposal(&p)?;
    }

    store.update_admin_proposal_status("prop-ls-002", "accepted", "admin", None)?;
    store.update_admin_proposal_status("prop-ls-004", "rejected", "admin", None)?;

    let open = store.list_admin_proposals(Some("open"), None, 100)?;
    assert_eq!(open.len(), 3);

    let accepted = store.list_admin_proposals(Some("accepted"), None, 100)?;
    assert_eq!(accepted.len(), 1);

    let rejected = store.list_admin_proposals(Some("rejected"), None, 100)?;
    assert_eq!(rejected.len(), 1);

    let limited = store.list_admin_proposals(None, None, 2)?;
    assert_eq!(limited.len(), 2);

    Ok(())
}

#[test]
fn test_admin_proposal_create_tool() -> anyhow::Result<()> {
    let temp_dir = tempdir()?;
    let store = Arc::new(GatewayStore::open(temp_dir.path())?);
    let registry = default_registry();
    let manifest = test_manifest_with_approval_queue();
    let policy = PolicyEngine::new(manifest.clone());

    let result = registry.execute(
        "admin_proposal_create",
        &manifest,
        &policy,
        temp_dir.path(),
        None,
        &json!({
            "title": "Agents lack structured HTTP response parsing tool",
            "category": "tool",
            "evidence": {"sessions": 12, "pattern": "json_parse_failure"},
            "remediation": "Add a structured response parser tool",
            "blast_radius": "medium",
            "priority": "high"
        })
        .to_string(),
        None,
        None,
        None,
        Some(store.clone()),
        None,
    )?;
    let parsed: serde_json::Value = serde_json::from_str(&result)?;
    assert_eq!(parsed["ok"], true);
    assert_eq!(parsed["deduped"], false);
    assert!(parsed["proposal_id"].as_str().unwrap().starts_with("prop-"));

    let proposals = store.list_admin_proposals(None, None, 100)?;
    assert_eq!(proposals.len(), 1);

    Ok(())
}

#[test]
fn test_admin_proposal_create_tool_dedup() -> anyhow::Result<()> {
    let temp_dir = tempdir()?;
    let store = Arc::new(GatewayStore::open(temp_dir.path())?);
    let registry = default_registry();
    let manifest = test_manifest_with_approval_queue();
    let policy = PolicyEngine::new(manifest.clone());

    let args = json!({
        "title": "Network timeout recurring pattern",
        "category": "protocol",
        "evidence": {"count": 5},
        "remediation": "Add retry logic",
        "blast_radius": "low"
    });

    let r1 = registry.execute(
        "admin_proposal_create",
        &manifest,
        &policy,
        temp_dir.path(),
        None,
        &args.to_string(),
        None,
        None,
        None,
        Some(store.clone()),
        None,
    )?;
    let p1: serde_json::Value = serde_json::from_str(&r1)?;
    assert_eq!(p1["ok"], true);

    let r2 = registry.execute(
        "admin_proposal_create",
        &manifest,
        &policy,
        temp_dir.path(),
        None,
        &json!({
            "title": "Network timeout recurring pattern",
            "category": "protocol",
            "evidence": {"count": 8},
            "remediation": "Updated remediation: add exponential backoff",
            "blast_radius": "low",
            "priority": "critical"
        })
        .to_string(),
        None,
        None,
        None,
        Some(store.clone()),
        None,
    )?;
    let p2: serde_json::Value = serde_json::from_str(&r2)?;
    assert_eq!(p2["ok"], true);
    assert_eq!(p2["deduped"], true);
    assert_eq!(p2["proposal_id"], p1["proposal_id"]);

    let proposals = store.list_admin_proposals(None, None, 100)?;
    assert_eq!(proposals.len(), 1);

    Ok(())
}

#[test]
fn test_admin_proposal_create_tool_denied_without_approval_queue() -> anyhow::Result<()> {
    let registry = default_registry();
    let manifest = test_manifest_no_caps();
    let available = registry.available_definitions(&manifest);
    let create_tool = available.iter().find(|d| d.name == "admin_proposal_create");
    assert!(create_tool.is_none());

    Ok(())
}

#[test]
fn test_admin_proposal_list_tool() -> anyhow::Result<()> {
    let temp_dir = tempdir()?;
    let store = Arc::new(GatewayStore::open(temp_dir.path())?);

    for i in 0..3 {
        let p = make_proposal(
            &format!("prop-lt-{i:03}"),
            &format!("List test {i}"),
            "tool",
        );
        store.insert_admin_proposal(&p)?;
    }

    let registry = default_registry();
    let manifest = test_manifest_with_read_access();
    let policy = PolicyEngine::new(manifest.clone());

    let result = registry.execute(
        "admin_proposal_list",
        &manifest,
        &policy,
        temp_dir.path(),
        None,
        &json!({}).to_string(),
        None,
        None,
        None,
        Some(store.clone()),
        None,
    )?;
    let parsed: serde_json::Value = serde_json::from_str(&result)?;
    assert_eq!(parsed["ok"], true);
    assert_eq!(parsed["count"], 3);

    let filtered = registry.execute(
        "admin_proposal_list",
        &manifest,
        &policy,
        temp_dir.path(),
        None,
        &json!({"status": "open"}).to_string(),
        None,
        None,
        None,
        Some(store.clone()),
        None,
    )?;
    let f: serde_json::Value = serde_json::from_str(&filtered)?;
    assert_eq!(f["count"], 3);

    Ok(())
}

#[test]
fn test_admin_proposal_create_validation() -> anyhow::Result<()> {
    let temp_dir = tempdir()?;
    let store = Arc::new(GatewayStore::open(temp_dir.path())?);
    let registry = default_registry();
    let manifest = test_manifest_with_approval_queue();
    let policy = PolicyEngine::new(manifest.clone());

    let bad_category = registry.execute(
        "admin_proposal_create",
        &manifest,
        &policy,
        temp_dir.path(),
        None,
        &json!({
            "title": "Test",
            "category": "invalid_category",
            "evidence": {},
            "remediation": "Fix",
            "blast_radius": "low"
        })
        .to_string(),
        None,
        None,
        None,
        Some(store.clone()),
        None,
    )?;
    let r: serde_json::Value = serde_json::from_str(&bad_category)?;
    assert_eq!(r["ok"], false);

    let bad_blast = registry.execute(
        "admin_proposal_create",
        &manifest,
        &policy,
        temp_dir.path(),
        None,
        &json!({
            "title": "Test",
            "category": "tool",
            "evidence": {},
            "remediation": "Fix",
            "blast_radius": "nuclear"
        })
        .to_string(),
        None,
        None,
        None,
        Some(store.clone()),
        None,
    )?;
    let r: serde_json::Value = serde_json::from_str(&bad_blast)?;
    assert_eq!(r["ok"], false);

    Ok(())
}
