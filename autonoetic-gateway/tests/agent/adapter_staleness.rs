//! Composition drift on the roster (#1202): `agent_list` and `agent_inspect`
//! surface wrapper `adapter` provenance plus a computed `stale_base` verdict —
//! the base was re-promoted (or removed) after the wrapper's mapping was
//! generated. Advisory signal only; nothing here blocks a spawn.

use autonoetic_gateway::policy::PolicyEngine;
use autonoetic_gateway::runtime::tools::agent::AgentListTool;
use autonoetic_gateway::runtime::tools::agent_inspect::AgentInspectTool;
use autonoetic_gateway::runtime::tools::NativeTool;
use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_types::agent::AdapterProvenance;
use autonoetic_types::capability::Capability;
use autonoetic_types::agent_revision::{AgentRevisionRecord, AgentRevisionStatus};
use std::path::Path;
use std::sync::Arc;
use crate::support::manifest_builder::TestManifest;

const BASE_REV_A: &str = "rev_sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const BASE_REV_B: &str = "rev_sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

fn caller_manifest() -> autonoetic_types::agent::AgentManifest {
    TestManifest::new()
        .capabilities(vec![Capability::SandboxFunctions {
            allowed: vec!["*".to_string()],
        }])
        .build()
}

fn minimal_skill(agent_id: &str, adapter_block: Option<&str>) -> String {
    format!(
        r#"---
version: "1.0"
runtime:
  engine: "autonoetic"
  gateway_version: "0.1.0"
  sdk_version: "0.1.0"
  type: "stateful"
  sandbox: "bubblewrap"
  runtime_lock: "runtime.lock"
agent:
  id: "{agent_id}"
  name: "{agent_id}"
  description: "roster staleness fixture"
{}---
# Fixture
"#,
        adapter_block.unwrap_or("")
    )
}

fn insert_and_promote(
    store: &GatewayStore,
    gateway_dir: &Path,
    agent_id: &str,
    revision_id: &str,
    skill_md: &str,
    manifest_summary: Option<serde_json::Value>,
) {
    let rev = AgentRevisionRecord {
        revision_id: revision_id.to_string(),
        agent_id: agent_id.to_string(),
        base_revision_id: None,
        artifact_id: None,
        content_digest: format!("sha256:{}", &revision_id[10..]),
        runtime_lock_hash: "sha256:0".to_string(),
        manifest_hash: "sha256:0".to_string(),
        created_at: "2026-08-28T00:00:00Z".to_string(),
        created_by_type: "test".to_string(),
        created_by_id: "test".to_string(),
        requested_by_type: None,
        requested_by_id: None,
        source_kind: "test".to_string(),
        source_ref: None,
        origin_node_id: "test-node".to_string(),
        trust_domain: "local".to_string(),
        status: AgentRevisionStatus::Ready,
        metadata_json: match manifest_summary {
            Some(m) => serde_json::json!({ "manifest": m }),
            None => serde_json::json!({}),
        },
        short_id: revision_id[10..18].to_string(),
        detected_network_hosts: None,
        signature: None,
        signer_id: None,
    };
    store.insert_agent_revision_transactional(&rev).unwrap();
    let dir = autonoetic_gateway::agent::agent_revision_dir(gateway_dir, agent_id, revision_id);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("SKILL.md"), skill_md).unwrap();
    store
        .atomic_promote(
            agent_id,
            revision_id,
            &format!("promo-{revision_id}"),
            "test",
            "test",
            Some("staleness fixture"),
            None,
            None,
        )
        .unwrap();
}

fn summary_with_adapter(provenance: &AdapterProvenance) -> serde_json::Value {
    serde_json::json!({
        "description": "roster staleness fixture",
        "capabilities": [],
        "execution_mode": "reasoning",
        "io": null,
        "adapter": serde_json::to_value(provenance).unwrap(),
    })
}

fn provenance(claimed: Option<&str>) -> AdapterProvenance {
    AdapterProvenance {
        base_agent_id: "base.agent".to_string(),
        base_revision_digest: claimed.map(|s| s.to_string()),
        generated_at: None,
        schema_notes: vec![],
        generator: Some("agent-adapter.default".to_string()),
    }
}

fn list_agents(store: &Arc<GatewayStore>, gateway_dir: &Path) -> Vec<serde_json::Value> {
    let tool = AgentListTool;
    let manifest = caller_manifest();
    let policy = PolicyEngine::new(manifest.clone());
    let out = tool
        .execute(
            &manifest,
            &policy,
            gateway_dir,
            Some(gateway_dir),
            "{}",
            Some("sess-roster"),
            None,
            None,
            Some(store.clone()),
            None,
        )
        .unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(parsed["ok"], true);
    parsed["agents"].as_array().unwrap().clone()
}

fn find<'a>(agents: &'a [serde_json::Value], id: &str) -> &'a serde_json::Value {
    agents
        .iter()
        .find(|a| a["agent_id"] == id)
        .unwrap_or_else(|| panic!("'{id}' should be listed, got: {agents:?}"))
}

/// SQLite-summary path: a wrapper whose claimed digest matches the base's
/// promoted revision is current; after the base moves, the same entry flips
/// to `stale_base: true` — no wrapper rebuild involved.
#[test]
fn agent_list_reports_stale_base_from_summary() {
    let temp = tempfile::tempdir().unwrap();
    let gateway_dir = temp.path().join(".gateway");
    std::fs::create_dir_all(&gateway_dir).unwrap();
    let store = Arc::new(GatewayStore::open(&gateway_dir).unwrap());

    insert_and_promote(
        &store,
        &gateway_dir,
        "base.agent",
        BASE_REV_A,
        &minimal_skill("base.agent", None),
        Some(serde_json::json!({"description": "base", "capabilities": [], "execution_mode": "reasoning"})),
    );
    insert_and_promote(
        &store,
        &gateway_dir,
        "wrapper.agent",
        "rev_sha256:cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc",
        &minimal_skill("wrapper.agent", None),
        Some(summary_with_adapter(&provenance(Some(BASE_REV_A)))),
    );

    let agents = list_agents(&store, &gateway_dir);
    let wrapper = find(&agents, "wrapper.agent");
    assert_eq!(wrapper["adapter"]["base_agent_id"], "base.agent");
    assert_eq!(wrapper["adapter"]["generator"], "agent-adapter.default");
    assert_eq!(wrapper["stale_base"], false);
    // A plain agent carries neither.
    assert_eq!(find(&agents, "base.agent")["adapter"], serde_json::Value::Null);
    assert_eq!(find(&agents, "base.agent")["stale_base"], serde_json::Value::Null);

    // Base moves on — the untouched wrapper is now stale.
    insert_and_promote(
        &store,
        &gateway_dir,
        "base.agent",
        BASE_REV_B,
        &minimal_skill("base.agent", None),
        Some(serde_json::json!({"description": "base", "capabilities": [], "execution_mode": "reasoning"})),
    );
    let agents = list_agents(&store, &gateway_dir);
    assert_eq!(find(&agents, "wrapper.agent")["stale_base"], true);
}

/// SKILL.md fallback path (no SQLite manifest summary): provenance parsed
/// from the revision's SKILL.md drives the same verdict.
#[test]
fn agent_list_reports_stale_base_from_skill_fallback() {
    let temp = tempfile::tempdir().unwrap();
    let gateway_dir = temp.path().join(".gateway");
    std::fs::create_dir_all(&gateway_dir).unwrap();
    let store = Arc::new(GatewayStore::open(&gateway_dir).unwrap());

    insert_and_promote(
        &store,
        &gateway_dir,
        "base.agent",
        BASE_REV_A,
        &minimal_skill("base.agent", None),
        None,
    );
    insert_and_promote(
        &store,
        &gateway_dir,
        "wrapper.agent",
        "rev_sha256:dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd",
        &minimal_skill(
            "wrapper.agent",
            Some(&format!(
                "adapter:\n  base_agent_id: \"base.agent\"\n  base_revision_digest: \"{BASE_REV_B}\"\n"
            )),
        ),
        None,
    );

    let agents = list_agents(&store, &gateway_dir);
    let wrapper = find(&agents, "wrapper.agent");
    assert_eq!(wrapper["adapter"]["base_agent_id"], "base.agent");
    assert_eq!(wrapper["stale_base"], true);
}

/// `agent_inspect` on the wrapper: provenance + verdict under `skill`.
#[test]
fn agent_inspect_reports_adapter_and_stale_base() {
    let temp = tempfile::tempdir().unwrap();
    let gateway_dir = temp.path().join(".gateway");
    std::fs::create_dir_all(&gateway_dir).unwrap();
    let store = Arc::new(GatewayStore::open(&gateway_dir).unwrap());

    insert_and_promote(
        &store,
        &gateway_dir,
        "base.agent",
        BASE_REV_A,
        &minimal_skill("base.agent", None),
        None,
    );
    insert_and_promote(
        &store,
        &gateway_dir,
        "wrapper.agent",
        "rev_sha256:eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee",
        &minimal_skill(
            "wrapper.agent",
            Some(&format!(
                "adapter:\n  base_agent_id: \"base.agent\"\n  base_revision_digest: \"{BASE_REV_A}\"\n"
            )),
        ),
        None,
    );

    let tool = AgentInspectTool;
    let manifest = caller_manifest();
    let policy = PolicyEngine::new(manifest.clone());
    let out = tool
        .execute(
            &manifest,
            &policy,
            &gateway_dir,
            Some(&gateway_dir),
            r#"{"agent_id":"wrapper.agent"}"#,
            Some("sess-roster"),
            None,
            None,
            Some(store.clone()),
            None,
        )
        .unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(parsed["ok"], true, "{parsed}");
    assert_eq!(parsed["skill"]["adapter"]["base_agent_id"], "base.agent");
    assert_eq!(parsed["skill"]["stale_base"], false);
}
