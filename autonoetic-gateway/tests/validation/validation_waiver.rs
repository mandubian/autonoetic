
use std::sync::Arc;

use autonoetic_gateway::policy::PolicyEngine;
use autonoetic_gateway::runtime::content_store::ContentStore;
use autonoetic_gateway::runtime::tools::default_registry;
use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_gateway::scheduler::workflow_store;
use autonoetic_types::agent::{AgentIdentity, AgentManifest};
use autonoetic_types::capability::Capability;
use autonoetic_types::config::GatewayConfig;
use serde_json::json;
use tempfile::tempdir;
use crate::support::manifest_builder::TestManifest;

fn planner_manifest() -> AgentManifest {
    AgentManifest {
        agent: AgentIdentity {
            id: "planner.collaborative".to_string(),
            name: "Collaborative Planner".to_string(),
            description: "Test planner".to_string(),
            singleton: false,
            resident_idle_ttl_secs: None,
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
        ..TestManifest::new().build()
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
        "validation_id": "lint",
        "validation_class": "quality_check",
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
    assert_eq!(waivers[0].validation_id, "lint");
    assert_eq!(waivers[0].validation_class, autonoetic_types::plan_frame::ValidationClass::QualityCheck);
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
    assert_eq!(list_v["waivers"][0]["validation_id"], "lint");
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
    assert!(v["message"].as_str().unwrap().contains("cannot be waived"));

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
    assert!(v["message"].as_str().unwrap().contains("cannot be waived"));
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
        "validation_id": "lint",
        "validation_class": "quality_check",
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
    let err = v["message"].as_str().unwrap();
    assert!(err.contains("art_*"), "error should mention art_*: {}", err);

    let waivers = store.list_waivers_for_workflow("").unwrap();
    assert_eq!(waivers.len(), 0, "no waiver should be persisted");
}

fn build_artifact_with_files(
    registry: &autonoetic_gateway::runtime::tools::NativeToolRegistry,
    manifest: &AgentManifest,
    policy: &PolicyEngine,
    agent_dir: &std::path::Path,
    gateway_dir: &std::path::Path,
    config: &GatewayConfig,
    store: &Arc<GatewayStore>,
    session_id: &str,
    files: &[(&str, &[u8])],
) -> String {
    let cs = autonoetic_gateway::runtime::content_store::ContentStore::new(gateway_dir).unwrap();
    let mut input_names = Vec::new();
    for (name, content) in files {
        let h = cs.write(content).unwrap();
        cs.register_name(session_id, name, &h).unwrap();
        input_names.push(name.to_string());
    }

    let args = serde_json::json!({ "inputs": input_names });
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
    v["artifact_ref"]
        .as_str()
        .unwrap_or_else(|| panic!("no artifact_ref in response: {:?}", v))
        .to_string()
}

fn project_and_modify_workbench(
    registry: &autonoetic_gateway::runtime::tools::NativeToolRegistry,
    manifest: &AgentManifest,
    policy: &PolicyEngine,
    agent_dir: &std::path::Path,
    gateway_dir: &std::path::Path,
    config: &GatewayConfig,
    store: &Arc<GatewayStore>,
    session_id: &str,
    artifact_ref: &str,
) -> String {
    let project_out = registry
        .execute(
            "artifact_project",
            manifest,
            policy,
            agent_dir,
            Some(gateway_dir),
            &serde_json::to_string(&json!({ "artifact_ref": artifact_ref })).unwrap(),
            Some(session_id),
            None,
            Some(config),
            Some(store.clone()),
            None,
        )
        .unwrap();
    let project_v: serde_json::Value = serde_json::from_str(&project_out).unwrap();
    assert_eq!(project_v["ok"], true, "project should succeed: {:?}", project_v);
    let workbench_id = project_v["workbench_id"].as_str().unwrap().to_string();
    let workspace_path = project_v["workspace_path"].as_str().unwrap();

    // Make a trivial edit so reconcile has something to commit.
    let hello = std::path::Path::new(workspace_path).join("hello.txt");
    std::fs::write(&hello, b"modified").unwrap();

    workbench_id
}

#[test]
fn promotion_query_returns_waived_validations() {
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

    let session_id = "root-session-waiver-promo";
    let artifact_id = make_artifact(
        &registry, &manifest, &policy, &agent_dir,
        &gateway_dir, &config, &store, session_id,
    );

    // Record a waiver.
    let waive_args = json!({
        "artifact_id": artifact_id,
        "validation_id": "lint",
        "validation_class": "quality_check",
        "reason": "doc-only change"
    });
    let waive_out = registry
        .execute(
            "validation_waive",
            &manifest,
            &policy,
            &agent_dir,
            Some(&gateway_dir),
            &waive_args.to_string(),
            Some(session_id),
            None,
            Some(&config),
            Some(store.clone()),
            None,
        )
        .unwrap();
    let waive_v: serde_json::Value = serde_json::from_str(&waive_out).unwrap();
    assert_eq!(waive_v["ok"], true, "waiver should succeed: {:?}", waive_v);

    // promotion.query needs a promotion record to attach waiver metadata to.
    // Use the unit_test_runner role to record a passing verdict.
    let mut unit_test_manifest = manifest.clone();
    unit_test_manifest.agent.id = "unit_test_runner.default".to_string();
    let unit_test_policy = PolicyEngine::new(unit_test_manifest.clone());
    crate::support::promotion_trace::seed_success_trace(
        store.as_ref(),
        session_id,
        "trace-waiver-promo-001",
    );
    let record_args = json!({
        "artifact_id": artifact_id,
        "role": "unit_test_runner",
        "pass": true,
        "findings": [],
        "summary": "tests waived",
        "execution_trace_id": "trace-waiver-promo-001"
    });
    let record_out = registry
        .execute(
            "promotion_record",
            &unit_test_manifest,
            &unit_test_policy,
            &agent_dir,
            Some(&gateway_dir),
            &record_args.to_string(),
            Some(session_id),
            None,
            Some(&config),
            Some(store.clone()),
            None,
        )
        .unwrap();
    let record_v: serde_json::Value = serde_json::from_str(&record_out).unwrap();
    assert_eq!(record_v["ok"], true, "promotion_record should succeed: {:?}", record_v);

    // promotion.query should surface the waiver alongside the record.
    let query_out = registry
        .execute(
            "promotion_query",
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
    let query_v: serde_json::Value = serde_json::from_str(&query_out).unwrap();
    assert!(
        query_v.get("waived_validations").is_some(),
        "promotion.query should include waived_validations: {:?}",
        query_v
    );
    let waived = query_v["waived_validations"].as_array().unwrap();
    assert_eq!(waived.len(), 1, "expected one waived validation: {:?}", waived);
    assert_eq!(waived[0]["validation_id"], "lint");
    assert_eq!(waived[0]["validation_class"], "quality_check");
    assert_eq!(waived[0]["reason"], "doc-only change");
}

#[test]
fn workbench_reconcile_propose_waivers_false_by_default() {
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

    let session_id = "root-session-waiver-propose-default";
    let artifact_ref = build_artifact_with_files(
        &registry, &manifest, &policy, &agent_dir,
        &gateway_dir, &config, &store, session_id,
        &[("hello.txt", b"hello")],
    );
    let wb_id = project_and_modify_workbench(
        &registry, &manifest, &policy, &agent_dir,
        &gateway_dir, &config, &store, session_id, &artifact_ref,
    );

    let reconcile_args = json!({ "workbench_id": wb_id, "message": "test" });
    let out = registry
        .execute(
            "workbench_reconcile",
            &manifest,
            &policy,
            &agent_dir,
            Some(&gateway_dir),
            &reconcile_args.to_string(),
            Some(session_id),
            None,
            Some(&config),
            Some(store.clone()),
            None,
        )
        .unwrap();
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["ok"], true, "reconcile should succeed: {:?}", v);
    assert_eq!(
        v["propose_waivers"], false,
        "propose_waivers should be false by default: {:?}",
        v
    );

    let provenance = v["provenance"].clone();
    assert_eq!(
        provenance["propose_waivers"], false,
        "provenance should record propose_waivers=false: {:?}",
        provenance
    );
}

#[test]
fn workbench_reconcile_propose_waivers_true_when_config_enabled() {
    let dir = tempdir().unwrap();
    let mut config = make_config(dir.path());
    config.validation_waivers.enabled = true;
    config.validation_waivers.auto_propose_after_reconcile = true;
    let gateway_dir = dir.path().join(".gateway");
    std::fs::create_dir_all(&gateway_dir).unwrap();
    let store = Arc::new(GatewayStore::open(&gateway_dir).unwrap());
    let registry = default_registry();
    let manifest = planner_manifest();
    let policy = PolicyEngine::new(manifest.clone());
    let agent_dir = dir.path().join("planner.collaborative");
    std::fs::create_dir_all(&agent_dir).unwrap();

    let session_id = "root-session-waiver-propose-enabled";
    let artifact_ref = build_artifact_with_files(
        &registry, &manifest, &policy, &agent_dir,
        &gateway_dir, &config, &store, session_id,
        &[("hello.txt", b"hello")],
    );
    let wb_id = project_and_modify_workbench(
        &registry, &manifest, &policy, &agent_dir,
        &gateway_dir, &config, &store, session_id, &artifact_ref,
    );

    let reconcile_args = json!({ "workbench_id": wb_id, "message": "test" });
    let out = registry
        .execute(
            "workbench_reconcile",
            &manifest,
            &policy,
            &agent_dir,
            Some(&gateway_dir),
            &reconcile_args.to_string(),
            Some(session_id),
            None,
            Some(&config),
            Some(store.clone()),
            None,
        )
        .unwrap();
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["ok"], true, "reconcile should succeed: {:?}", v);
    assert_eq!(
        v["propose_waivers"], true,
        "propose_waivers should be true when enabled in config: {:?}",
        v
    );

    let provenance = v["provenance"].clone();
    assert_eq!(
        provenance["propose_waivers"], true,
        "provenance should record propose_waivers=true: {:?}",
        provenance
    );
}

#[test]
fn correctness_check_waiver_from_agent_requires_operator() {
    // #1144 pin: agents can never self-waive a correctness gate — the denial
    // must be the precise `correctness_waiver_requires_operator` (naming the
    // operator path), never the generic non_waivable_validation that the
    // is_waivable inversion used to produce.
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

    let session_id = "root-session-waiver-correctness";
    let artifact_id = make_artifact(
        &registry, &manifest, &policy, &agent_dir,
        &gateway_dir, &config, &store, session_id,
    );

    let args = json!({
        "artifact_id": artifact_id,
        "validation_id": "unit_tests",
        "validation_class": "correctness_check",
        "reason": "tests are flaky on this runner"
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
    assert_eq!(v["ok"], false, "agents must not self-waive correctness: {v}");
    assert_eq!(
        v["error"].as_str().unwrap(),
        "correctness_waiver_requires_operator",
        "the precise operator-path denial, not non_waivable_validation: {v}"
    );
    assert!(
        v["repair_hint"].as_str().unwrap_or("").contains("operator"),
        "the hint must route to the operator: {v}"
    );
    assert!(
        store.list_waivers_for_artifact(&artifact_id).unwrap().is_empty(),
        "no waiver row may be written"
    );
}
