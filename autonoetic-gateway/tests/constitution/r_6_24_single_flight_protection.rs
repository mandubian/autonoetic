//! Constitution P-6.24: equivalent durable operations must coalesce instead of
//! faning out into parallel work.


use autonoetic_gateway::policy::PolicyEngine;
use autonoetic_gateway::runtime::tools::default_registry;
use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_types::agent::{AgentIdentity, AgentManifest};
use autonoetic_types::capability::Capability;
use autonoetic_types::config::GatewayConfig;
use std::sync::Arc;
use tempfile::tempdir;
use crate::support::manifest_builder::TestManifest;

fn planner_manifest() -> AgentManifest {
    AgentManifest {
        agent: AgentIdentity {
            id: "planner.default".to_string(),
            name: "planner.default".to_string(),
            description: "test".to_string(),
            singleton: false,
            resident_idle_ttl_secs: None,
        },
        capabilities: vec![Capability::AgentSpawn {
            max_children: 2,
            max_spawn_depth: 0,
        }],
        ..TestManifest::new().build()
    }
}

#[test]
fn duplicate_durable_spawns_coalesce_to_one_active_task() {
    let manifest = planner_manifest();
    let policy = PolicyEngine::new(manifest.clone());
    let temp = tempdir().expect("tempdir should create");
    let agents_dir = temp.path().join("agents");
    let parent_dir = agents_dir.join("planner.default");
    let child_dir = agents_dir.join("builder.default");
    std::fs::create_dir_all(&parent_dir).expect("parent dir should create");
    std::fs::create_dir_all(&child_dir).expect("child dir should create");

    let config = GatewayConfig {
        agents_dir: agents_dir.clone(),
        ..GatewayConfig::default()
    };
    let gateway_dir = autonoetic_gateway::execution::gateway_root_dir(&config);
    let store = Arc::new(GatewayStore::open(&gateway_dir).expect("gateway store should open"));

    let args = serde_json::json!({
        "agent_id": "builder.default",
        "message": "Build the durable artifact and keep the workflow state authoritative.",
        "metadata": {
            "stage_kind": "durable_build",
            "artifact_ref": "ar.test-build-01"
        }
    });

    let registry = default_registry();
    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime should create");
    let _guard = runtime.enter();

    let first = registry
        .execute(
            "agent_spawn",
            &manifest,
            &policy,
            &parent_dir,
            Some(&gateway_dir),
            &args.to_string(),
            Some("root-r624-single-flight"),
            None,
            Some(&config),
            Some(store.clone()),
            None,
        )
        .expect("first durable spawn should queue");
    let first_json: serde_json::Value =
        serde_json::from_str(&first).expect("first response should decode");
    let workflow_id = first_json["workflow_id"]
        .as_str()
        .expect("workflow_id should be present")
        .to_string();
    let task_id = first_json["task_id"]
        .as_str()
        .expect("task_id should be present")
        .to_string();

    let second = registry
        .execute(
            "agent_spawn",
            &manifest,
            &policy,
            &parent_dir,
            Some(&gateway_dir),
            &args.to_string(),
            Some("root-r624-single-flight"),
            None,
            Some(&config),
            Some(store.clone()),
            None,
        )
        .expect("duplicate durable spawn should coalesce");
    let second_json: serde_json::Value =
        serde_json::from_str(&second).expect("coalesced response should decode");

    assert_eq!(second_json["status"].as_str(), Some("coalesced"));
    assert_eq!(second_json["existing_task_id"].as_str(), Some(task_id.as_str()));
    assert_eq!(second_json["retry_advice"].as_str(), Some("wait"));
    assert!(second_json["dedupe_key"].as_str().is_some());

    let events = store
        .list_workflow_events(&workflow_id)
        .expect("workflow events should load");
    let coalesced = events
        .iter()
        .find(|event| event.event_type == "workflow.single_flight.coalesced")
        .expect("workflow.single_flight.coalesced event should exist");
    assert_eq!(coalesced.payload["status"].as_str(), Some("coalesced"));
    assert_eq!(coalesced.payload["existing_task_id"].as_str(), Some(task_id.as_str()));
    assert_eq!(coalesced.payload["retry_advice"].as_str(), Some("wait"));
}
