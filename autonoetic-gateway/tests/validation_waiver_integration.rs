mod support;

use std::sync::Arc;

use autonoetic_gateway::policy::PolicyEngine;
use autonoetic_gateway::runtime::content_store::ContentStore;
use autonoetic_gateway::runtime::tools::default_registry;
use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_gateway::scheduler::workflow_store;
use autonoetic_types::agent::{AgentIdentity, AgentManifest, RuntimeDeclaration};
use autonoetic_types::capability::Capability;
use autonoetic_types::config::GatewayConfig;
use serde_json::json;
use tempfile::tempdir;

fn planner_manifest() -> AgentManifest {
    AgentManifest {
        version: "1.0".to_string(),
        runtime: RuntimeDeclaration {
            engine: "autonoetic".to_string(),
            gateway_version: "0.1.0".to_string(),
            sdk_version: "0.1.0".to_string(),
            runtime_type: "stateful".to_string(),
            sandbox: "bubblewrap".to_string(),
            runtime_lock: "runtime.lock".to_string(),
        },
        agent: AgentIdentity {
            id: "planner.collaborative".to_string(),
            name: "Collaborative Planner".to_string(),
            description: "Test planner".to_string(),
        },
        capabilities: vec![
            Capability::WriteAccess {
                scopes: vec!["*".to_string()],
            },
            Capability::ReadAccess {
                scopes: vec!["*".to_string()],
            },
            Capability::AgentSpawn {
                max_children: 10,
                max_spawn_depth: 0,
            },
            Capability::PlanFrameAccess {
                patterns: vec!["*".to_string()],
            },
        ],
        llm_overrides: None,
        llm_preset: None,
        llm_config: None,
        limits: None,
        background: None,
        disclosure: None,
        io: None,
        middleware: None,
        execution_mode: Default::default(),
        script_entry: None,
        script_input_mode: Default::default(),
        gateway_url: None,
        gateway_token: None,
        allowed_tool_tiers: vec![],
        agentskills_import: None,
        compression: None,
        sandbox_network: autonoetic_types::agent::SandboxNetworkPolicy::default(),
    }
}

fn make_config(dir: &std::path::Path) -> GatewayConfig {
    let mut config = GatewayConfig::default();
    config.agents_dir = dir.to_path_buf();
    config
}

fn make_artifact(
    registry: &autonoetic_gateway::runtime::tools::NativeToolRegistry,
    manifest: &AgentManifest,
    policy: &PolicyEngine,
    agent_dir: &std::path::Path,
    gateway_dir: &std::path::Path,
    config: &GatewayConfig,
    store: &Arc<GatewayStore>,
    session_id: &str,
) -> String {
    let cs = ContentStore::new(gateway_dir).unwrap();
    let h = cs.write(b"test content").unwrap();
    cs.register_name(session_id, "test.txt", &h).unwrap();

    let args = serde_json::json!({ "inputs": ["test.txt"] });
    let out = registry
        .execute(
            "artifact_build",
            manifest,
            policy,
            agent_dir,
            Some(gateway_dir),
            &args.to_string(),
            Some(session_id),
            None,
            Some(config),
            Some(store.clone()),
            None,
        )
        .unwrap();
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    if v["ok"] != true {
        panic!("artifact_build failed: {:?}", v);
    }
    let ref_id = v["artifact_ref"]
        .as_str()
        .unwrap_or_else(|| panic!("no artifact_ref in response: {:?}", v))
        .to_string();
    let record = store
        .resolve_artifact_ref_any_scope(&ref_id, session_id)
        .unwrap()
        .unwrap_or_else(|| panic!("artifact_ref {} did not resolve", ref_id));
    record.artifact_id
}

#[test]
fn validation_waive_records_persists_and_lists() {
    let dir = tempdir().unwrap();
    let config = make_config(dir.path());
    let gateway_dir = dir.path().join(".gateway");
    std::fs::create_dir_all(&gateway_dir).unwrap();
    let store = Arc::new(GatewayStore::open(&gateway_dir).unwrap());
    let registry = default_registry();
    let manifest = planner_manifest();
    let policy = PolicyEngine::new(manifest.clone());
    let agent_dir = dir.path().join("planner.collaborative");
    std::fs::create_dir_all(&agent_dir).unwrap();

    let session_id = "root-session-waiver-001";
    let artifact_id = make_artifact(
        &registry, &manifest, &policy, &agent_dir,
        &gateway_dir, &config, &store, session_id,
    );

    let args = json!({
        "artifact_id": artifact_id,
        "validation_id": "unit_tests",
        "validation_class": "correctness_check",
        "reason": "Small prompt-only SKILL.md edit; no executable code changed"
    });
    let out = registry
        .execute(
            "validation_waive",
            &manifest,
            &policy,
            &agent_dir,
            Some(&gateway_dir),
            &args.to_string(),
            Some(session_id),
            None,
            Some(&config),
            Some(store.clone()),
            None,
        )
        .unwrap();
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["ok"], true, "waiver should succeed: {:?}", v);
    let waiver_id = v["waiver_id"].as_str().unwrap();
    assert!(waiver_id.starts_with("vw-"));

    let waivers = store.list_waivers_for_artifact(&artifact_id).unwrap();
    assert_eq!(waivers.len(), 1);
    assert_eq!(waivers[0].validation_id, "unit_tests");
    assert_eq!(waivers[0].validation_class, autonoetic_types::plan_frame::ValidationClass::CorrectnessCheck);
    assert_eq!(waivers[0].waived_by, manifest.agent.id);
    assert!(waivers[0].reason.contains("prompt-only"));

    let list_out = registry
        .execute(
            "validation_waivers",
            &manifest,
            &policy,
            &agent_dir,
            Some(&gateway_dir),
            &json!({ "artifact_id": artifact_id }).to_string(),
            Some(session_id),
            None,
            Some(&config),
            Some(store.clone()),
            None,
        )
        .unwrap();
    let list_v: serde_json::Value = serde_json::from_str(&list_out).unwrap();
    assert_eq!(list_v["count"], 1);
    assert_eq!(list_v["waivers"][0]["validation_id"], "unit_tests");
}

#[test]
fn validation_waive_rejects_mechanical_safety() {
    let dir = tempdir().unwrap();
    let config = make_config(dir.path());
    let gateway_dir = dir.path().join(".gateway");
    std::fs::create_dir_all(&gateway_dir).unwrap();
    let store = Arc::new(GatewayStore::open(&gateway_dir).unwrap());
    let registry = default_registry();
    let manifest = planner_manifest();
    let policy = PolicyEngine::new(manifest.clone());
    let agent_dir = dir.path().join("planner.collaborative");
    std::fs::create_dir_all(&agent_dir).unwrap();

    let session_id = "root-session-waiver-002";
    let artifact_id = make_artifact(
        &registry, &manifest, &policy, &agent_dir,
        &gateway_dir, &config, &store, session_id,
    );

    let args = json!({
        "artifact_id": artifact_id,
        "validation_id": "capability_enforcement",
        "validation_class": "mechanical_safety",
        "reason": "trying to skip safety gate"
    });
    let out = registry
        .execute(
            "validation_waive",
            &manifest,
            &policy,
            &agent_dir,
            Some(&gateway_dir),
            &args.to_string(),
            Some(session_id),
            None,
            Some(&config),
            Some(store.clone()),
            None,
        )
        .unwrap();
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["ok"], false);
    assert!(v["error"].as_str().unwrap().contains("cannot be waived"));

    let waivers = store.list_waivers_for_artifact(&artifact_id).unwrap();
    assert_eq!(waivers.len(), 0, "mechanical_safety waiver must NOT be stored");
}

#[test]
fn validation_waive_rejects_security_review() {
    let dir = tempdir().unwrap();
    let config = make_config(dir.path());
    let gateway_dir = dir.path().join(".gateway");
    std::fs::create_dir_all(&gateway_dir).unwrap();
    let store = Arc::new(GatewayStore::open(&gateway_dir).unwrap());
    let registry = default_registry();
    let manifest = planner_manifest();
    let policy = PolicyEngine::new(manifest.clone());
    let agent_dir = dir.path().join("planner.collaborative");
    std::fs::create_dir_all(&agent_dir).unwrap();

    let session_id = "root-session-waiver-003";
    let artifact_id = make_artifact(
        &registry, &manifest, &policy, &agent_dir,
        &gateway_dir, &config, &store, session_id,
    );

    let args = json!({
        "artifact_id": artifact_id,
        "validation_id": "auditor_review",
        "validation_class": "security_review",
        "reason": "trying to skip security"
    });
    let out = registry
        .execute(
            "validation_waive",
            &manifest,
            &policy,
            &agent_dir,
            Some(&gateway_dir),
            &args.to_string(),
            Some(session_id),
            None,
            Some(&config),
            Some(store.clone()),
            None,
        )
        .unwrap();
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["ok"], false);
    assert!(v["error"].as_str().unwrap().contains("cannot be waived"));
}

#[test]
fn validation_waive_rejects_empty_reason() {
    let dir = tempdir().unwrap();
    let config = make_config(dir.path());
    let gateway_dir = dir.path().join(".gateway");
    std::fs::create_dir_all(&gateway_dir).unwrap();
    let store = Arc::new(GatewayStore::open(&gateway_dir).unwrap());
    let registry = default_registry();
    let manifest = planner_manifest();
    let policy = PolicyEngine::new(manifest.clone());
    let agent_dir = dir.path().join("planner.collaborative");
    std::fs::create_dir_all(&agent_dir).unwrap();

    let session_id = "root-session-waiver-004";
    let artifact_id = make_artifact(
        &registry, &manifest, &policy, &agent_dir,
        &gateway_dir, &config, &store, session_id,
    );

    let args = json!({
        "artifact_id": artifact_id,
        "validation_id": "unit_tests",
        "validation_class": "correctness_check",
        "reason": "   "
    });
    let out = registry
        .execute(
            "validation_waive",
            &manifest,
            &policy,
            &agent_dir,
            Some(&gateway_dir),
            &args.to_string(),
            Some(session_id),
            None,
            Some(&config),
            Some(store.clone()),
            None,
        )
        .unwrap();
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["ok"], false);
    assert!(v["error"].as_str().unwrap().contains("reason"));
}

#[test]
fn validation_waivers_listed_for_workflow() {
    let dir = tempdir().unwrap();
    let config = make_config(dir.path());
    let gateway_dir = dir.path().join(".gateway");
    std::fs::create_dir_all(&gateway_dir).unwrap();
    let store = Arc::new(GatewayStore::open(&gateway_dir).unwrap());
    let registry = default_registry();
    let manifest = planner_manifest();
    let policy = PolicyEngine::new(manifest.clone());
    let agent_dir = dir.path().join("planner.collaborative");
    std::fs::create_dir_all(&agent_dir).unwrap();

    let session_id = "root-session-waiver-005";

    let _workflow = workflow_store::ensure_workflow_for_root_session(
        &config, Some(&store), session_id, Some(&manifest.agent.id),
    ).unwrap();

    let artifact_id = make_artifact(
        &registry, &manifest, &policy, &agent_dir,
        &gateway_dir, &config, &store, session_id,
    );

    let args = json!({
        "artifact_id": artifact_id,
        "validation_id": "style_review",
        "validation_class": "quality_check",
        "reason": "pre-existing style violations"
    });
    registry
        .execute(
            "validation_waive",
            &manifest,
            &policy,
            &agent_dir,
            Some(&gateway_dir),
            &args.to_string(),
            Some(session_id),
            None,
            Some(&config),
            Some(store.clone()),
            None,
        )
        .unwrap();

    let wf = workflow_store::resolve_workflow_id_for_root_session(
        &config, session_id,
    ).ok().flatten();
    let workflow_id = wf.unwrap_or_default();
    let workflow_waivers = store.list_waivers_for_workflow(&workflow_id).unwrap();
    assert_eq!(workflow_waivers.len(), 1);
    assert_eq!(workflow_waivers[0].validation_id, "style_review");
}

#[test]
fn validation_waive_rejects_non_art_artifact_id() {
    let dir = tempdir().unwrap();
    let config = make_config(dir.path());
    let gateway_dir = dir.path().join(".gateway");
    std::fs::create_dir_all(&gateway_dir).unwrap();
    let store = Arc::new(GatewayStore::open(&gateway_dir).unwrap());
    let registry = default_registry();
    let manifest = planner_manifest();
    let policy = PolicyEngine::new(manifest.clone());
    let agent_dir = dir.path().join("planner.collaborative");
    std::fs::create_dir_all(&agent_dir).unwrap();

    let session_id = "root-session-waiver-006";
    let _ = make_artifact(
        &registry, &manifest, &policy, &agent_dir,
        &gateway_dir, &config, &store, session_id,
    );

    let args = json!({
        "artifact_id": "ar.abc123def456",
        "validation_id": "unit_tests",
        "validation_class": "correctness_check",
        "reason": "trying to pass a ref instead of an id"
    });
    let out = registry
        .execute(
            "validation_waive",
            &manifest,
            &policy,
            &agent_dir,
            Some(&gateway_dir),
            &args.to_string(),
            Some(session_id),
            None,
            Some(&config),
            Some(store.clone()),
            None,
        )
        .unwrap();
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["ok"], false);
    let err = v["error"].as_str().unwrap();
    assert!(err.contains("art_*"), "error should mention art_*: {}", err);

    let waivers = store.list_waivers_for_workflow("").unwrap();
    assert_eq!(waivers.len(), 0, "no waiver should be persisted");
}
