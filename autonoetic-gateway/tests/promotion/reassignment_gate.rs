//! Slot-reassignment approval gate (issue #658) + smoke-test-for-shape-change
//! requirement (issue #657).
//!
//! A promotion into an EXISTING agent slot may be a categorical reassignment —
//! replacing the agent that occupies the slot with a different kind of agent —
//! rather than an in-place version bump. These tests verify the gateway:
//!
//!   * requires a distinct operator approval when the slot is reassigned
//!     (execution_mode / script entrypoint / unrelated description) or when the
//!     capability surface shrinks — even when no capability broadened (#658);
//!   * surfaces the reassignment detail (what changed) in the approval payload;
//!   * does NOT require the reassignment approval for a true in-place upgrade;
//!   * requires a smoke test for a shape-changing replacement of an executable
//!     agent, the way it would for a brand-new agent (#657).

use autonoetic_gateway::policy::PolicyEngine;
use autonoetic_gateway::runtime::tools::default_registry;
use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_gateway::scheduler::{approve_request_with_options, ApproveOptions};
use autonoetic_types::agent::{AgentIdentity, AgentManifest};
use autonoetic_types::agent_revision::{
    AgentAliasRecord, AgentRevisionRecord, AgentRevisionStatus,
};
use autonoetic_types::background::ApprovalLevel;
use autonoetic_types::capability::Capability;
use autonoetic_types::config::GatewayConfig;
use autonoetic_types::principal::PrincipalKind;
use std::sync::Arc;
use tempfile::tempdir;
use crate::support::manifest_builder::TestManifest;

const AGENT_ID: &str = "reassign-agent";
const OUTGOING_REVISION: &str = "rev_outgoing";
const INCOMING_REVISION: &str = "rev_incoming";

fn manifest_with_revision_cap() -> AgentManifest {
    AgentManifest {
        agent: AgentIdentity {
            id: AGENT_ID.to_string(),
            name: AGENT_ID.to_string(),
            description: "test".to_string(),
            singleton: false,
            resident_idle_ttl_secs: None,
        },
        capabilities: vec![Capability::AgentRevision {
            patterns: vec!["*".to_string()],
        }],
        ..TestManifest::new().build()
    }
}

/// Build a canonical SKILL.md whose shape fields (execution_mode, script_entry,
/// description) nest under `metadata.autonoetic`, matching what the gateway
/// composes for installed agents — so `RevisionShape` parses them reliably.
fn skill_md(
    description: &str,
    execution_mode: Option<&str>,
    script_entry: Option<&str>,
    capabilities: &[Capability],
) -> String {
    let mut caps_yaml = String::new();
    if !capabilities.is_empty() {
        caps_yaml.push_str("    capabilities:\n");
        for cap in capabilities {
            let json = serde_json::to_value(cap).unwrap();
            let as_yaml = serde_yaml::to_string(&json).unwrap();
            let mut first = true;
            for line in as_yaml.trim_end().lines() {
                if first {
                    caps_yaml.push_str("      - ");
                    caps_yaml.push_str(line);
                    caps_yaml.push('\n');
                    first = false;
                } else {
                    caps_yaml.push_str("        ");
                    caps_yaml.push_str(line);
                    caps_yaml.push('\n');
                }
            }
        }
    }
    let exec_line = execution_mode
        .map(|m| format!("    execution_mode: {}\n", m))
        .unwrap_or_default();
    let entry_line = script_entry
        .map(|e| format!("    script_entry: {}\n", e))
        .unwrap_or_default();
    format!(
        "---\n\
         name: \"{id}\"\n\
         description: \"{desc}\"\n\
         metadata:\n\
         \x20\x20autonoetic:\n\
         \x20\x20\x20\x20version: \"1.0\"\n\
         \x20\x20\x20\x20runtime:\n\
         \x20\x20\x20\x20\x20\x20engine: autonoetic\n\
         \x20\x20\x20\x20\x20\x20gateway_version: \"0.1.0\"\n\
         \x20\x20\x20\x20\x20\x20sdk_version: \"0.1.0\"\n\
         \x20\x20\x20\x20\x20\x20type: stateful\n\
         \x20\x20\x20\x20\x20\x20sandbox: bubblewrap\n\
         \x20\x20\x20\x20\x20\x20runtime_lock: runtime.lock\n\
         \x20\x20\x20\x20agent:\n\
         \x20\x20\x20\x20\x20\x20id: {id}\n\
         \x20\x20\x20\x20\x20\x20name: {id}\n\
         \x20\x20\x20\x20\x20\x20description: \"{desc}\"\n\
{exec}{entry}{caps}---\n\
         # body\n",
        id = AGENT_ID,
        desc = description,
        exec = exec_line,
        entry = entry_line,
        caps = caps_yaml,
    )
}

struct Harness {
    _temp: tempfile::TempDir,
    store: Arc<GatewayStore>,
    agent_dir: std::path::PathBuf,
    gateway_dir: std::path::PathBuf,
}

fn write_revision(
    gateway_dir: &std::path::Path,
    revision_id: &str,
    description: &str,
    execution_mode: Option<&str>,
    script_entry: Option<&str>,
    capabilities: &[Capability],
) {
    let dir = gateway_dir
        .join("revisions/agents")
        .join(AGENT_ID)
        .join(revision_id);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("SKILL.md"),
        skill_md(description, execution_mode, script_entry, capabilities),
    )
    .unwrap();
}

fn make_revision_record(revision_id: &str) -> AgentRevisionRecord {
    AgentRevisionRecord {
        revision_id: revision_id.to_string(),
        agent_id: AGENT_ID.to_string(),
        base_revision_id: None,
        artifact_id: None,
        content_digest: format!("sha256:{}", revision_id),
        runtime_lock_hash: "sha256:lock".to_string(),
        manifest_hash: "sha256:manifest".to_string(),
        created_at: chrono::Utc::now().to_rfc3339(),
        created_by_type: PrincipalKind::Human.tag().to_string(),
        created_by_id: "test".to_string(),
        requested_by_type: None,
        requested_by_id: None,
        source_kind: "intent".to_string(),
        source_ref: None,
        origin_node_id: "local".to_string(),
        trust_domain: "local".to_string(),
        status: AgentRevisionStatus::Candidate,
        metadata_json: serde_json::Value::Null,
        short_id: revision_id.chars().take(8).collect(),
        detected_network_hosts: None,
        signature: None,
        signer_id: None,
    }
}

/// Outgoing revision is a reasoning agent; incoming is a script agent with a
/// wholly different description and the SAME (zero) capabilities — so there is
/// no capability broadening at all, yet the slot is being reassigned.
fn setup_reasoning_to_script() -> Harness {
    let temp = tempdir().unwrap();
    let agents_dir = temp.path().join("agents");
    let agent_dir = agents_dir.join(AGENT_ID);
    let gateway_dir = agents_dir.join(".gateway");
    std::fs::create_dir_all(&agent_dir).unwrap();
    std::fs::create_dir_all(&gateway_dir).unwrap();
    let store = Arc::new(GatewayStore::open(&gateway_dir).unwrap());

    write_revision(
        &gateway_dir,
        OUTGOING_REVISION,
        "Produces tested minimal auditable code for reuse",
        None,
        None,
        &[],
    );
    write_revision(
        &gateway_dir,
        INCOMING_REVISION,
        "Fibonacci sequence calculator with persisted state",
        Some("script"),
        Some("fibonacci.py"),
        &[],
    );
    store
        .insert_agent_revision(&make_revision_record(OUTGOING_REVISION))
        .unwrap();
    store
        .insert_agent_revision(&make_revision_record(INCOMING_REVISION))
        .unwrap();
    store
        .upsert_agent_alias(&AgentAliasRecord {
            alias_id: AGENT_ID.to_string(),
            agent_id: AGENT_ID.to_string(),
            revision_id: OUTGOING_REVISION.to_string(),
            updated_at: chrono::Utc::now().to_rfc3339(),
            updated_by_type: PrincipalKind::Human.tag().to_string(),
            updated_by_id: "test".to_string(),
            reason: None,
            suspended_at: None,
            suspended_reason: None,
            suspended_by: None,
        })
        .unwrap();

    Harness {
        _temp: temp,
        store,
        agent_dir,
        gateway_dir,
    }
}

fn invoke_promote(h: &Harness, args_json: &str) -> serde_json::Value {
    let manifest = manifest_with_revision_cap();
    let policy = PolicyEngine::new(manifest.clone());
    let registry = default_registry();
    let raw = registry
        .execute(
            "agent_revision_promote",
            &manifest,
            &policy,
            &h.agent_dir,
            Some(&h.gateway_dir),
            args_json,
            Some("test-session"),
            Some("turn-000001"),
            None,
            Some(h.store.clone()),
            None,
        )
        .expect("execute should not error for normal cases");
    serde_json::from_str(&raw).expect("response is JSON")
}

#[test]
fn reasoning_to_script_replacement_requires_reassignment_approval() {
    // The incident scenario: replace a reasoning coder with a script agent
    // under the SAME agent_id and the SAME (empty) capability set. No
    // capability broadened, yet this is a categorical slot reassignment and
    // must require operator approval (#658).
    let h = setup_reasoning_to_script();
    let result = invoke_promote(
        &h,
        &format!(
            r#"{{"agent_id":"{}","revision_id":"{}"}}"#,
            AGENT_ID, INCOMING_REVISION
        ),
    );

    assert_eq!(result["ok"], false, "unexpected: {:?}", result);
    assert_eq!(result["error"], "capability_delta_requires_approval");
    assert_eq!(result["approval_required"], true);

    let reass = &result["delta"]["reassignment"];
    assert_eq!(
        reass["slot_reassignment"], true,
        "execution_mode reasoning→script must classify as a slot reassignment: {:?}",
        reass,
    );
    assert_eq!(reass["execution_mode_changed"], true);
    // script_entry_changed only fires for a script→script entrypoint swap; a
    // reasoning→script transition is already captured by execution_mode_changed.
    assert_eq!(reass["script_entry_changed"], false);
    assert_eq!(reass["description_unrelated"], true);

    // The human-facing message must surface the reassignment severity so the
    // operator can give informed consent.
    let msg = result["message"].as_str().unwrap_or("");
    assert!(
        msg.contains("Slot reassignment"),
        "approval message must call out the slot reassignment: {:?}",
        msg,
    );
}

#[test]
fn unrelated_description_alone_triggers_reassignment_approval() {
    // Same execution_mode, same capabilities, but a wholly unrelated
    // description ⇒ the slot is being handed to a different role.
    let temp = tempdir().unwrap();
    let agents_dir = temp.path().join("agents");
    let agent_dir = agents_dir.join(AGENT_ID);
    let gateway_dir = agents_dir.join(".gateway");
    std::fs::create_dir_all(&agent_dir).unwrap();
    std::fs::create_dir_all(&gateway_dir).unwrap();
    let store = Arc::new(GatewayStore::open(&gateway_dir).unwrap());
    write_revision(
        &gateway_dir,
        OUTGOING_REVISION,
        "Researches topics and writes summaries",
        None,
        None,
        &[],
    );
    write_revision(
        &gateway_dir,
        INCOMING_REVISION,
        "Deploys containers to the production cluster",
        None,
        None,
        &[],
    );
    store
        .insert_agent_revision(&make_revision_record(OUTGOING_REVISION))
        .unwrap();
    store
        .insert_agent_revision(&make_revision_record(INCOMING_REVISION))
        .unwrap();
    store
        .upsert_agent_alias(&AgentAliasRecord {
            alias_id: AGENT_ID.to_string(),
            agent_id: AGENT_ID.to_string(),
            revision_id: OUTGOING_REVISION.to_string(),
            updated_at: chrono::Utc::now().to_rfc3339(),
            updated_by_type: PrincipalKind::Human.tag().to_string(),
            updated_by_id: "test".to_string(),
            reason: None,
            suspended_at: None,
            suspended_reason: None,
            suspended_by: None,
        })
        .unwrap();
    let h = Harness {
        _temp: temp,
        store,
        agent_dir,
        gateway_dir,
    };

    let result = invoke_promote(
        &h,
        &format!(
            r#"{{"agent_id":"{}","revision_id":"{}"}}"#,
            AGENT_ID, INCOMING_REVISION
        ),
    );

    assert_eq!(result["ok"], false, "unexpected: {:?}", result);
    assert_eq!(result["error"], "capability_delta_requires_approval");
    let reass = &result["delta"]["reassignment"];
    assert_eq!(reass["slot_reassignment"], true);
    assert_eq!(reass["description_unrelated"], true);
    assert_eq!(reass["execution_mode_changed"], false);
}

#[test]
fn pure_capability_shrinkage_requires_approval() {
    // Outgoing has two capabilities, incoming keeps only one. No broadening,
    // so the old gate returned None and the promote proceeded with NO approval.
    // #658: capability removal/narrowing now requires an approval too.
    let temp = tempdir().unwrap();
    let agents_dir = temp.path().join("agents");
    let agent_dir = agents_dir.join(AGENT_ID);
    let gateway_dir = agents_dir.join(".gateway");
    std::fs::create_dir_all(&agent_dir).unwrap();
    std::fs::create_dir_all(&gateway_dir).unwrap();
    let store = Arc::new(GatewayStore::open(&gateway_dir).unwrap());
    let write_cap = Capability::WriteAccess {
        scopes: vec!["self.*".to_string()],
    };
    let read_cap = Capability::ReadAccess {
        scopes: vec!["self.*".to_string()],
    };
    write_revision(
        &gateway_dir,
        OUTGOING_REVISION,
        "same role",
        None,
        None,
        &[write_cap.clone(), read_cap],
    );
    write_revision(
        &gateway_dir,
        INCOMING_REVISION,
        "same role",
        None,
        None,
        &[write_cap],
    );
    store
        .insert_agent_revision(&make_revision_record(OUTGOING_REVISION))
        .unwrap();
    store
        .insert_agent_revision(&make_revision_record(INCOMING_REVISION))
        .unwrap();
    store
        .upsert_agent_alias(&AgentAliasRecord {
            alias_id: AGENT_ID.to_string(),
            agent_id: AGENT_ID.to_string(),
            revision_id: OUTGOING_REVISION.to_string(),
            updated_at: chrono::Utc::now().to_rfc3339(),
            updated_by_type: PrincipalKind::Human.tag().to_string(),
            updated_by_id: "test".to_string(),
            reason: None,
            suspended_at: None,
            suspended_reason: None,
            suspended_by: None,
        })
        .unwrap();
    let h = Harness {
        _temp: temp,
        store,
        agent_dir,
        gateway_dir,
    };

    let result = invoke_promote(
        &h,
        &format!(
            r#"{{"agent_id":"{}","revision_id":"{}"}}"#,
            AGENT_ID, INCOMING_REVISION
        ),
    );

    assert_eq!(result["ok"], false, "unexpected: {:?}", result);
    assert_eq!(result["error"], "capability_delta_requires_approval");
    let reass = &result["delta"]["reassignment"];
    assert_eq!(reass["slot_reassignment"], false);
    let removed: Vec<&str> = reass["removed_capabilities"]
        .as_array()
        .map(|v| v.iter().filter_map(|x| x.as_str()).collect())
        .unwrap_or_default();
    assert!(
        removed.iter().any(|c| *c == "ReadAccess"),
        "removed_capabilities must include ReadAccess: {:?}",
        removed,
    );
}

#[test]
fn in_place_upgrade_does_not_trigger_reassignment_approval() {
    // Regression guard: identical shape + capabilities is an in-place upgrade,
    // not a reassignment. No reassignment approval, and (zero-cap) it promotes.
    let temp = tempdir().unwrap();
    let agents_dir = temp.path().join("agents");
    let agent_dir = agents_dir.join(AGENT_ID);
    let gateway_dir = agents_dir.join(".gateway");
    std::fs::create_dir_all(&agent_dir).unwrap();
    std::fs::create_dir_all(&gateway_dir).unwrap();
    let store = Arc::new(GatewayStore::open(&gateway_dir).unwrap());
    write_revision(
        &gateway_dir,
        OUTGOING_REVISION,
        "same role",
        None,
        None,
        &[],
    );
    write_revision(
        &gateway_dir,
        INCOMING_REVISION,
        "same role",
        None,
        None,
        &[],
    );
    store
        .insert_agent_revision(&make_revision_record(OUTGOING_REVISION))
        .unwrap();
    store
        .insert_agent_revision(&make_revision_record(INCOMING_REVISION))
        .unwrap();
    store
        .upsert_agent_alias(&AgentAliasRecord {
            alias_id: AGENT_ID.to_string(),
            agent_id: AGENT_ID.to_string(),
            revision_id: OUTGOING_REVISION.to_string(),
            updated_at: chrono::Utc::now().to_rfc3339(),
            updated_by_type: PrincipalKind::Human.tag().to_string(),
            updated_by_id: "test".to_string(),
            reason: None,
            suspended_at: None,
            suspended_reason: None,
            suspended_by: None,
        })
        .unwrap();
    let h = Harness {
        _temp: temp,
        store,
        agent_dir,
        gateway_dir,
    };

    let result = invoke_promote(
        &h,
        &format!(
            r#"{{"agent_id":"{}","revision_id":"{}"}}"#,
            AGENT_ID, INCOMING_REVISION
        ),
    );

    assert_eq!(result["ok"], true, "unexpected: {:?}", result);
    assert_eq!(result["status"], "promoted");
}

#[test]
fn reassignment_approval_resolved_then_promotes() {
    // Full flow: first call mints the reassignment approval; after the operator
    // approves, retrying with `approval_ref` bypasses the gate and promotes.
    let h = setup_reasoning_to_script();

    let first = invoke_promote(
        &h,
        &format!(
            r#"{{"agent_id":"{}","revision_id":"{}"}}"#,
            AGENT_ID, INCOMING_REVISION
        ),
    );
    assert_eq!(first["error"], "capability_delta_requires_approval");
    let approval_ref = first["approval_ref"].as_str().unwrap().to_string();

    let mut cfg = GatewayConfig::default();
    cfg.approval_dwell_multiplier = 0.0;
    let apr_row = h.store.get_approval(&approval_ref).unwrap().unwrap();
    approve_request_with_options(
        &cfg,
        Some(&h.store),
        &approval_ref,
        "operator",
        Some("approved reassignment for test".to_string()),
        None,
        Some(&ApprovalLevel::Operator),
        None,
        ApproveOptions {
            confirm_phrase: apr_row.confirm_phrase.clone(),
            ..Default::default()
        },
    )
    .expect("operator approval should succeed");

    let retry = invoke_promote(
        &h,
        &format!(
            r#"{{"agent_id":"{}","revision_id":"{}","approval_ref":"{}"}}"#,
            AGENT_ID, INCOMING_REVISION, approval_ref
        ),
    );
    assert_eq!(retry["ok"], true, "unexpected: {:?}", retry);
    assert_eq!(retry["status"], "promoted");
    assert_eq!(retry["installed"], true);
}
