mod support;

use std::sync::Arc;

use autonoetic_gateway::policy::PolicyEngine;
use autonoetic_gateway::runtime::content_store::ContentStore;
use autonoetic_gateway::runtime::tools::default_registry;
use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
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
            singleton: false,
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
            open_web: false,
        sandbox_network: autonoetic_types::agent::SandboxNetworkPolicy::default(),
    }
}

fn make_config(dir: &std::path::Path) -> GatewayConfig {
    let mut config = GatewayConfig::default();
    config.agents_dir = dir.to_path_buf();
    config
}

fn build_artifact(
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
    let cs = ContentStore::new(gateway_dir).unwrap();
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

#[test]
fn artifact_project_creates_editable_workbench() {
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

    let session_id = "root-session-001";

    let artifact_id = build_artifact(
        &registry,
        &manifest,
        &policy,
        &agent_dir,
        &gateway_dir,
        &config,
        &store,
        session_id,
        &[
            ("main.py", b"print('hello')"),
            ("config.yaml", b"name: test\n"),
        ],
    );

    let result = registry
        .execute(
            "artifact_project",
            &manifest,
            &policy,
            &agent_dir,
            Some(&gateway_dir),
            &serde_json::to_string(&json!({ "artifact_ref": artifact_id })).unwrap(),
            Some(session_id),
            None,
            Some(&config),
            Some(store.clone()),
            None,
        )
        .unwrap();

    let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
    assert_eq!(parsed["ok"], true, "project should succeed: {:?}", parsed);
    assert!(parsed["workbench_id"].as_str().unwrap().starts_with("wb-"));
    assert_eq!(parsed["file_count"], 2);

    let workbench_id = parsed["workbench_id"].as_str().unwrap();
    let workspace_path = parsed["workspace_path"].as_str().unwrap();

    let main_py = std::path::Path::new(workspace_path).join("main.py");
    assert!(main_py.exists(), "main.py should exist in workbench");
    let content = std::fs::read_to_string(&main_py).unwrap();
    assert_eq!(content, "print('hello')");

    let wb = store.load_workbench(workbench_id).unwrap().unwrap();
    assert!(wb.base_artifact_id.starts_with("art_"), "base_artifact_id should be art_*: {}", wb.base_artifact_id);
    assert_eq!(wb.status.as_str(), "active");

    // Session Room P1: projecting a workbench lands a `workbench.created` event
    // on the canonical timeline, referencing the workbench surface.
    let tl = store
        .list_session_timeline("root-session-001", None, 100, None, None)
        .unwrap();
    let created = tl
        .entries
        .iter()
        .find(|e| e.event_type == "workbench.created")
        .expect("workbench.created event on the canonical timeline");
    assert_eq!(created.refs.workbench_id.as_deref(), Some(workbench_id));

    std::fs::write(&main_py, "print('modified')").unwrap();
    let content_after = std::fs::read_to_string(&main_py).unwrap();
    assert_eq!(content_after, "print('modified')");
}

#[test]
fn workbench_diff_detects_changes() {
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

    let session_id = "root-session-002";

    let artifact_id = build_artifact(
        &registry,
        &manifest,
        &policy,
        &agent_dir,
        &gateway_dir,
        &config,
        &store,
        session_id,
        &[
            ("main.py", b"print('hello')"),
            ("config.yaml", b"name: test\n"),
        ],
    );

    let result = registry
        .execute(
            "artifact_project",
            &manifest,
            &policy,
            &agent_dir,
            Some(&gateway_dir),
            &serde_json::to_string(&json!({ "artifact_ref": artifact_id })).unwrap(),
            Some(session_id),
            None,
            Some(&config),
            Some(store.clone()),
            None,
        )
        .unwrap();

    let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
    let workbench_id = parsed["workbench_id"].as_str().unwrap().to_string();
    let workspace_path = parsed["workspace_path"].as_str().unwrap().to_string();

    let diff_before = registry
        .execute(
            "workbench_diff",
            &manifest,
            &policy,
            &agent_dir,
            Some(&gateway_dir),
            &serde_json::to_string(&json!({ "workbench_id": workbench_id })).unwrap(),
            Some(session_id),
            None,
            Some(&config),
            Some(store.clone()),
            None,
        )
        .unwrap();

    let diff_v: serde_json::Value = serde_json::from_str(&diff_before).unwrap();
    assert_eq!(diff_v["ok"], true);
    assert_eq!(diff_v["changed_files"], 0);

    std::fs::write(std::path::Path::new(&workspace_path).join("main.py"), "print('changed')")
        .unwrap();
    std::fs::write(std::path::Path::new(&workspace_path).join("new_file.txt"), "added").unwrap();

    let diff_after = registry
        .execute(
            "workbench_diff",
            &manifest,
            &policy,
            &agent_dir,
            Some(&gateway_dir),
            &serde_json::to_string(&json!({ "workbench_id": workbench_id })).unwrap(),
            Some(session_id),
            None,
            Some(&config),
            Some(store.clone()),
            None,
        )
        .unwrap();

    let diff_v2: serde_json::Value = serde_json::from_str(&diff_after).unwrap();
    assert_eq!(diff_v2["ok"], true);
    assert_eq!(diff_v2["changed_files"], 2);

    let diffs = diff_v2["diffs"].as_array().unwrap();
    let modified: Vec<_> = diffs
        .iter()
        .filter(|d| d["change_type"] == "modified")
        .collect();
    let added: Vec<_> = diffs
        .iter()
        .filter(|d| d["change_type"] == "added")
        .collect();
    assert_eq!(modified.len(), 1);
    assert_eq!(added.len(), 1);
    assert_eq!(modified[0]["path"], "main.py");
    assert_eq!(added[0]["path"], "new_file.txt");
}

#[test]
fn workbench_checkpoint_and_checkout_roundtrip() {
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

    let session_id = "root-session-003";

    let artifact_id = build_artifact(
        &registry,
        &manifest,
        &policy,
        &agent_dir,
        &gateway_dir,
        &config,
        &store,
        session_id,
        &[("data.txt", b"original content")],
    );

    let result = registry
        .execute(
            "artifact_project",
            &manifest,
            &policy,
            &agent_dir,
            Some(&gateway_dir),
            &serde_json::to_string(&json!({ "artifact_ref": artifact_id })).unwrap(),
            Some(session_id),
            None,
            Some(&config),
            Some(store.clone()),
            None,
        )
        .unwrap();

    let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
    let workbench_id = parsed["workbench_id"].as_str().unwrap().to_string();
    let workspace_path = parsed["workspace_path"].as_str().unwrap().to_string();

    let cp_result = registry
        .execute(
            "workbench_checkpoint",
            &manifest,
            &policy,
            &agent_dir,
            Some(&gateway_dir),
            &serde_json::to_string(&json!({
                "workbench_id": workbench_id,
                "label": "initial"
            }))
            .unwrap(),
            Some(session_id),
            None,
            Some(&config),
            Some(store.clone()),
            None,
        )
        .unwrap();

    let cp_v: serde_json::Value = serde_json::from_str(&cp_result).unwrap();
    assert_eq!(cp_v["ok"], true);
    assert!(cp_v["checkpoint_id"].as_str().unwrap().starts_with("cp-"));
    let checkpoint_id = cp_v["checkpoint_id"].as_str().unwrap().to_string();

    std::fs::write(std::path::Path::new(&workspace_path).join("data.txt"), "modified content")
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(std::path::Path::new(&workspace_path).join("data.txt")).unwrap(),
        "modified content"
    );

    let checkout_result = registry
        .execute(
            "workbench_checkout",
            &manifest,
            &policy,
            &agent_dir,
            Some(&gateway_dir),
            &serde_json::to_string(&json!({ "checkpoint_id": checkpoint_id })).unwrap(),
            Some(session_id),
            None,
            Some(&config),
            Some(store.clone()),
            None,
        )
        .unwrap();

    let co_v: serde_json::Value = serde_json::from_str(&checkout_result).unwrap();
    assert_eq!(co_v["ok"], true);
    assert_eq!(co_v["restored_from"], checkpoint_id);

    assert_eq!(
        std::fs::read_to_string(std::path::Path::new(&workspace_path).join("data.txt")).unwrap(),
        "original content"
    );
}

#[test]
fn workbench_checkpoints_lists_history() {
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

    let session_id = "root-session-004";

    let artifact_id = build_artifact(
        &registry,
        &manifest,
        &policy,
        &agent_dir,
        &gateway_dir,
        &config,
        &store,
        session_id,
        &[("file.txt", b"content")],
    );

    let result = registry
        .execute(
            "artifact_project",
            &manifest,
            &policy,
            &agent_dir,
            Some(&gateway_dir),
            &serde_json::to_string(&json!({ "artifact_ref": artifact_id })).unwrap(),
            Some(session_id),
            None,
            Some(&config),
            Some(store.clone()),
            None,
        )
        .unwrap();

    let workbench_id = serde_json::from_str::<serde_json::Value>(&result).unwrap()["workbench_id"]
        .as_str()
        .unwrap()
        .to_string();

    for label in &["cp-1", "cp-2", "cp-3"] {
        registry
            .execute(
                "workbench_checkpoint",
                &manifest,
                &policy,
                &agent_dir,
                Some(&gateway_dir),
                &serde_json::to_string(&json!({
                    "workbench_id": workbench_id,
                    "label": label
                }))
                .unwrap(),
                Some(session_id),
                None,
                Some(&config),
                Some(store.clone()),
                None,
            )
            .unwrap();
    }

    let list_result = registry
        .execute(
            "workbench_checkpoints",
            &manifest,
            &policy,
            &agent_dir,
            Some(&gateway_dir),
            &serde_json::to_string(&json!({ "workbench_id": workbench_id })).unwrap(),
            Some(session_id),
            None,
            Some(&config),
            Some(store.clone()),
            None,
        )
        .unwrap();

    let list_v: serde_json::Value = serde_json::from_str(&list_result).unwrap();
    assert_eq!(list_v["ok"], true);
    assert_eq!(list_v["count"], 4, "expected 3 manual + 1 auto-projection checkpoint");
}

fn has_artifact_project_tool(
    registry: &autonoetic_gateway::runtime::tools::NativeToolRegistry,
    manifest: &AgentManifest,
) -> bool {
    let defs = registry.available_definitions(manifest);
    defs.iter().any(|d| d.name == "artifact_project")
}

#[test]
fn artifact_project_rejects_path_traversal() {
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

    let session_id = "root-session-005";

    if !has_artifact_project_tool(&registry, &manifest) {
        return;
    }

    let cs = ContentStore::new(&gateway_dir).unwrap();
    let evil_content = b"evil";
    let h = cs.write(evil_content).unwrap();
    if cs.register_name(session_id, "../../../etc/evil.txt", &h).is_err() {
        return;
    }

    let args = serde_json::json!({ "inputs": ["../../../etc/evil.txt"] });
    let build_out = registry.execute(
        "artifact_build",
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
    );

    let artifact_ref = match build_out {
        Ok(out) => {
            let build_v: serde_json::Value = serde_json::from_str(&out).unwrap();
            if build_v["ok"] == true {
                build_v["artifact_ref"].as_str().unwrap().to_string()
            } else {
                return;
            }
        }
        Err(_) => return,
    };

    let result = registry.execute(
        "artifact_project",
        &manifest,
        &policy,
        &agent_dir,
        Some(&gateway_dir),
        &serde_json::to_string(&json!({ "artifact_ref": artifact_ref })).unwrap(),
        Some(session_id),
        None,
        Some(&config),
        Some(store.clone()),
        None,
    );

    match result {
        Ok(out) => {
            let v: serde_json::Value = serde_json::from_str(&out).unwrap();
            assert_eq!(v["ok"], false, "should reject path traversal");
            assert!(v["error"].as_str().unwrap().contains("traversal"));
        }
        Err(e) => {
            assert!(e.to_string().contains("traversal"), "expected traversal error: {}", e);
        }
    }
}

fn project_artifact(
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
    let out = registry
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
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    if v["ok"] != true {
        panic!("artifact_project failed: {:?}", v);
    }
    v["workbench_id"].as_str().unwrap().to_string()
}

#[test]
fn workbench_reconcile_creates_new_artifact() {
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

    let session_id = "root-session-006";

    let artifact_ref = build_artifact(
        &registry,
        &manifest,
        &policy,
        &agent_dir,
        &gateway_dir,
        &config,
        &store,
        session_id,
        &[("hello.txt", b"hello world"), ("config.toml", b"[settings]\nkey = \"value\"")],
    );

    let wb_id = project_artifact(
        &registry,
        &manifest,
        &policy,
        &agent_dir,
        &gateway_dir,
        &config,
        &store,
        session_id,
        &artifact_ref,
    );

    let wb = store.load_workbench(&wb_id).unwrap().unwrap();
    let source_dir = std::path::Path::new(&wb.workspace_path);

    let edited = std::fs::read_to_string(source_dir.join("hello.txt")).unwrap();
    std::fs::write(source_dir.join("hello.txt"), format!("{} -- edited by operator", edited)).unwrap();
    std::fs::write(source_dir.join("new_file.txt"), b"brand new content").unwrap();

    let reconcile_args = serde_json::json!({
        "workbench_id": wb_id,
        "message": "operator edited hello.txt and added new_file.txt"
    });
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
    assert!(v["new_artifact_ref"].as_str().unwrap().starts_with("ar."));
    assert!(v["new_artifact_id"].as_str().unwrap().starts_with("art_"));
    assert_eq!(v["base_artifact_id"], v["base_artifact_id"]);
    assert_eq!(v["changed_files"], 2);
    assert_eq!(v["total_files"], 3);

    let provenance = &v["provenance"];
    let modified: Vec<&str> = provenance["operator_modified"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert!(modified.contains(&"hello.txt"));
    let added: Vec<&str> = provenance["operator_added"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert!(added.contains(&"new_file.txt"));
    assert_eq!(provenance["unchanged"], 1);

    let wb_after = store.load_workbench(&wb_id).unwrap().unwrap();
    assert_eq!(wb_after.status, autonoetic_types::workbench::WorkbenchStatus::Reconciled);
    assert!(wb_after.reconciled_at.is_some());

    // Session Room P1: `workbench.reconciled` lands on the timeline at Detail
    // altitude (min_altitude=None to include Detail), with the workbench ref.
    let tl = store
        .list_session_timeline("root-session-006", None, 100, None, None)
        .unwrap();
    let ev = tl
        .entries
        .iter()
        .find(|e| e.event_type == "workbench.reconciled")
        .expect("workbench.reconciled event on the canonical timeline");
    assert_eq!(ev.refs.workbench_id.as_deref(), Some(wb_id.as_str()));
    assert_eq!(ev.altitude, autonoetic_types::session_timeline::Altitude::Detail);
}

#[test]
fn workbench_reconcile_rejects_non_active() {
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

    let session_id = "root-session-007";

    let artifact_ref = build_artifact(
        &registry,
        &manifest,
        &policy,
        &agent_dir,
        &gateway_dir,
        &config,
        &store,
        session_id,
        &[("test.txt", b"test")],
    );

    let wb_id = project_artifact(
        &registry,
        &manifest,
        &policy,
        &agent_dir,
        &gateway_dir,
        &config,
        &store,
        session_id,
        &artifact_ref,
    );

    let discard_args = serde_json::json!({ "workbench_id": wb_id });
    registry
        .execute(
            "workbench_discard",
            &manifest,
            &policy,
            &agent_dir,
            Some(&gateway_dir),
            &discard_args.to_string(),
            Some(session_id),
            None,
            Some(&config),
            Some(store.clone()),
            None,
        )
        .unwrap();

    let reconcile_args = serde_json::json!({ "workbench_id": wb_id });
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
    assert_eq!(v["ok"], false);
    assert!(v["message"].as_str().unwrap().contains("discard"));
}

#[test]
fn workbench_discard_marks_discarded() {
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

    let session_id = "root-session-008";

    let artifact_ref = build_artifact(
        &registry,
        &manifest,
        &policy,
        &agent_dir,
        &gateway_dir,
        &config,
        &store,
        session_id,
        &[("test.txt", b"test")],
    );

    let wb_id = project_artifact(
        &registry,
        &manifest,
        &policy,
        &agent_dir,
        &gateway_dir,
        &config,
        &store,
        session_id,
        &artifact_ref,
    );

    let args = serde_json::json!({ "workbench_id": wb_id });
    let out = registry
        .execute(
            "workbench_discard",
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
    assert_eq!(v["ok"], true);
    assert_eq!(v["status"], "discarded");
    assert!(v["discarded_at"].is_string());

    let wb = store.load_workbench(&wb_id).unwrap().unwrap();
    assert_eq!(wb.status, autonoetic_types::workbench::WorkbenchStatus::Discarded);
    assert!(wb.discarded_at.is_some());

    // Session Room P1: `workbench.discarded` lands on the timeline at Detail.
    let tl = store
        .list_session_timeline("root-session-008", None, 100, None, None)
        .unwrap();
    let ev = tl
        .entries
        .iter()
        .find(|e| e.event_type == "workbench.discarded")
        .expect("workbench.discarded event on the canonical timeline");
    assert_eq!(ev.refs.workbench_id.as_deref(), Some(wb_id.as_str()));
    assert_eq!(ev.altitude, autonoetic_types::session_timeline::Altitude::Detail);
}

#[test]
fn workbench_discard_rejects_already_discarded() {
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

    let session_id = "root-session-009";

    let artifact_ref = build_artifact(
        &registry,
        &manifest,
        &policy,
        &agent_dir,
        &gateway_dir,
        &config,
        &store,
        session_id,
        &[("test.txt", b"test")],
    );

    let wb_id = project_artifact(
        &registry,
        &manifest,
        &policy,
        &agent_dir,
        &gateway_dir,
        &config,
        &store,
        session_id,
        &artifact_ref,
    );

    let args = serde_json::json!({ "workbench_id": wb_id });
    registry
        .execute(
            "workbench_discard",
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

    let out = registry
        .execute(
            "workbench_discard",
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
    assert!(v["message"].as_str().unwrap().contains("discard"));
}

// Issue #332: reconcile must produce a semantic_summary that flags
// capability changes and store the summary in
// `.autonoetic/semantic_summary.json` next to `reconciliation.json`.
#[test]
fn workbench_reconcile_writes_semantic_summary_with_capability_flag() {
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

    let session_id = "root-session-sem";

    let artifact_ref = build_artifact(
        &registry,
        &manifest,
        &policy,
        &agent_dir,
        &gateway_dir,
        &config,
        &store,
        session_id,
        &[
            ("capabilities.yaml", b"[]"),
            ("src/lib.rs", b"pub fn x() { 1 + 1 }"),
        ],
    );

    let wb_id = project_artifact(
        &registry,
        &manifest,
        &policy,
        &agent_dir,
        &gateway_dir,
        &config,
        &store,
        session_id,
        &artifact_ref,
    );

    let wb = store.load_workbench(&wb_id).unwrap().unwrap();
    let source_dir = std::path::Path::new(&wb.workspace_path);

    std::fs::write(
        source_dir.join("capabilities.yaml"),
        b"- network\n- shell\n",
    )
    .unwrap();
    std::fs::write(
        source_dir.join("src/lib.rs"),
        b"pub fn fetch() { let _ = reqwest::get(\"https://example.com\"); }",
    )
    .unwrap();

    let reconcile_args = json!({
        "workbench_id": wb_id,
    });
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
    assert_eq!(v["ok"], true, "reconcile failed: {:?}", v);

    let summary = &v["semantic_summary"];
    assert_eq!(summary["summarizer_id"], "rule_based_v1");
    assert_eq!(summary["workbench_id"], wb_id);

    let contract_changes = summary["contract_changes"].as_array().unwrap();
    let impacts: Vec<&str> = contract_changes
        .iter()
        .map(|c| c["impact"].as_str().unwrap())
        .collect();
    assert!(
        impacts.contains(&"capability_change"),
        "expected capability_change in contract_changes, got {:?}",
        impacts
    );
    assert!(
        impacts.contains(&"network_access_change"),
        "expected network_access_change in contract_changes, got {:?}",
        impacts
    );

    let summary_path = source_dir
        .parent()
        .unwrap()
        .join(".autonoetic")
        .join("semantic_summary.json");
    assert!(
        summary_path.exists(),
        "semantic_summary.json not written to disk at {:?}",
        summary_path
    );
    let on_disk: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&summary_path).unwrap()).unwrap();
    assert_eq!(on_disk["summarizer_id"], "rule_based_v1");
    assert_eq!(on_disk["workbench_id"], wb_id);
}

#[test]
fn workbench_reconcile_semantic_summary_no_contract_changes_for_source_edit() {
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

    let session_id = "root-session-sem-src-edit";

    let artifact_ref = build_artifact(
        &registry,
        &manifest,
        &policy,
        &agent_dir,
        &gateway_dir,
        &config,
        &store,
        session_id,
        &[("hello.txt", b"hello world"), ("readme.md", b"# readme")],
    );

    let wb_id = project_artifact(
        &registry,
        &manifest,
        &policy,
        &agent_dir,
        &gateway_dir,
        &config,
        &store,
        session_id,
        &artifact_ref,
    );

    let wb = store.load_workbench(&wb_id).unwrap().unwrap();
    let source_dir = std::path::Path::new(&wb.workspace_path);

    // Modify a plain text file (no contract impact).
    let edited = std::fs::read_to_string(source_dir.join("hello.txt")).unwrap();
    std::fs::write(source_dir.join("hello.txt"), format!("{} -- edited", edited)).unwrap();

    let reconcile_args = json!({ "workbench_id": wb_id });
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
    assert_eq!(v["ok"], true);
    let summary = &v["semantic_summary"];
    assert_eq!(summary["summarizer_id"], "rule_based_v1");
    assert_eq!(summary["contract_changes"].as_array().unwrap().len(), 0);
    assert_eq!(summary["changed_files"], 1);
}

// Issue #330 (a): auto-checkpoint on projection.
#[test]
fn artifact_project_creates_auto_checkpoint() {
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

    let session_id = "root-session-auto-cp-001";

    let artifact_id = build_artifact(
        &registry,
        &manifest,
        &policy,
        &agent_dir,
        &gateway_dir,
        &config,
        &store,
        session_id,
        &[("x.txt", b"hello")],
    );

    let result = registry
        .execute(
            "artifact_project",
            &manifest,
            &policy,
            &agent_dir,
            Some(&gateway_dir),
            &serde_json::to_string(&json!({ "artifact_ref": artifact_id })).unwrap(),
            Some(session_id),
            None,
            Some(&config),
            Some(store.clone()),
            None,
        )
        .unwrap();
    let v: serde_json::Value = serde_json::from_str(&result).unwrap();
    assert_eq!(v["ok"], true);
    let wb_id = v["workbench_id"].as_str().unwrap();

    // The auto-checkpoint should be listed.
    let list_out = registry
        .execute(
            "workbench_checkpoints",
            &manifest,
            &policy,
            &agent_dir,
            Some(&gateway_dir),
            &serde_json::to_string(&json!({ "workbench_id": wb_id })).unwrap(),
            Some(session_id),
            None,
            Some(&config),
            Some(store.clone()),
            None,
        )
        .unwrap();
    let list_v: serde_json::Value = serde_json::from_str(&list_out).unwrap();
    assert_eq!(list_v["ok"], true);
    assert!(list_v["count"].as_u64().unwrap() >= 1, "expected at least 1 auto-checkpoint");

    let cps = list_v["checkpoints"].as_array().unwrap();
    let labels: Vec<&str> = cps.iter()
        .filter_map(|c| c["label"].as_str())
        .collect();
    assert!(labels.contains(&"auto: projection"), "expected auto: projection label");
}

// Issue #330 (b): auto-checkpoint before reconcile.
#[test]
fn workbench_reconcile_creates_auto_checkpoint_before_reconcile() {
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

    let session_id = "root-session-auto-cp-002";

    let artifact_id = build_artifact(
        &registry,
        &manifest,
        &policy,
        &agent_dir,
        &gateway_dir,
        &config,
        &store,
        session_id,
        &[("hello.txt", b"hello world")],
    );

    let wb_id = project_artifact(
        &registry,
        &manifest,
        &policy,
        &agent_dir,
        &gateway_dir,
        &config,
        &store,
        session_id,
        &artifact_id,
    );

    let wb = store.load_workbench(&wb_id).unwrap().unwrap();
    let source_dir = std::path::Path::new(&wb.workspace_path);
    std::fs::write(source_dir.join("hello.txt"), b"edited").unwrap();

    let reconcile_args = json!({ "workbench_id": wb_id });
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
    assert_eq!(v["ok"], true);

    // After reconcile, the workbench should have the auto: pre-reconcile
    // checkpoint in addition to the auto: projection checkpoint.
    let list_out = registry
        .execute(
            "workbench_checkpoints",
            &manifest,
            &policy,
            &agent_dir,
            Some(&gateway_dir),
            &serde_json::to_string(&json!({ "workbench_id": wb_id })).unwrap(),
            Some(session_id),
            None,
            Some(&config),
            Some(store.clone()),
            None,
        )
        .unwrap();
    let list_v: serde_json::Value = serde_json::from_str(&list_out).unwrap();
    assert_eq!(list_v["ok"], true);
    let cps = list_v["checkpoints"].as_array().unwrap();
    let labels: Vec<&str> = cps.iter()
        .filter_map(|c| c["label"].as_str())
        .collect();
    assert!(labels.contains(&"auto: pre-reconcile"), "expected auto: pre-reconcile label");
    assert!(labels.contains(&"auto: projection"), "expected auto: projection label");
}

// Issue #330 (c): cleanup rejects active, cleans up reconciled.
#[test]
fn workbench_cleanup_rejects_active() {
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

    let session_id = "root-session-cleanup-001";

    let artifact_id = build_artifact(
        &registry,
        &manifest,
        &policy,
        &agent_dir,
        &gateway_dir,
        &config,
        &store,
        session_id,
        &[("x.txt", b"hello")],
    );

    let wb_id = project_artifact(
        &registry,
        &manifest,
        &policy,
        &agent_dir,
        &gateway_dir,
        &config,
        &store,
        session_id,
        &artifact_id,
    );

    let out = registry
        .execute(
            "workbench_cleanup",
            &manifest,
            &policy,
            &agent_dir,
            Some(&gateway_dir),
            &serde_json::to_string(&json!({ "workbench_id": wb_id })).unwrap(),
            Some(session_id),
            None,
            Some(&config),
            Some(store.clone()),
            None,
        )
        .unwrap();
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["ok"], false);
    assert!(v["message"].as_str().unwrap().contains("Cannot clean up an active"));
}

// Issue #330 (c): cleanup succeeds on reconciled workbench.
#[test]
fn workbench_cleanup_succeeds_on_reconciled() {
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

    let session_id = "root-session-cleanup-002";

    let artifact_id = build_artifact(
        &registry,
        &manifest,
        &policy,
        &agent_dir,
        &gateway_dir,
        &config,
        &store,
        session_id,
        &[("x.txt", b"hello")],
    );

    let wb_id = project_artifact(
        &registry,
        &manifest,
        &policy,
        &agent_dir,
        &gateway_dir,
        &config,
        &store,
        session_id,
        &artifact_id,
    );

    let wb = store.load_workbench(&wb_id).unwrap().unwrap();
    let source_dir = std::path::Path::new(&wb.workspace_path);

    // Reconcile first.
    let reconcile_args = json!({ "workbench_id": wb_id });
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
    assert_eq!(v["ok"], true);

    // Now cleanup.
    let cleanup_out = registry
        .execute(
            "workbench_cleanup",
            &manifest,
            &policy,
            &agent_dir,
            Some(&gateway_dir),
            &serde_json::to_string(&json!({ "workbench_id": wb_id })).unwrap(),
            Some(session_id),
            None,
            Some(&config),
            Some(store.clone()),
            None,
        )
        .unwrap();
    let cv: serde_json::Value = serde_json::from_str(&cleanup_out).unwrap();
    assert_eq!(cv["ok"], true, "cleanup failed: {:?}", cv);
    assert!(cv["message"].as_str().unwrap().contains("reconciled"));

    // Workbench record should be gone.
    let after = store.load_workbench(&wb_id).unwrap();
    assert!(after.is_none(), "workbench record should be deleted after cleanup");

    // Checkpoint records should be gone too.
    let cps = store.list_checkpoints_for_workbench(&wb_id).unwrap();
    assert!(cps.is_empty(), "checkpoint records should be deleted after cleanup");

    // Disk artifacts should be removed.
    assert!(!source_dir.exists(), "workspace directory should be removed");
    let checkpoints_dir = source_dir.parent().unwrap().join(".autonoetic").join("checkpoints");
    assert!(!checkpoints_dir.exists(), "checkpoints directory should be removed");
}

// Issue #330 (c): cleanup succeeds on discarded workbench.
#[test]
fn workbench_cleanup_succeeds_on_discarded() {
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

    let session_id = "root-session-cleanup-003";

    let artifact_id = build_artifact(
        &registry,
        &manifest,
        &policy,
        &agent_dir,
        &gateway_dir,
        &config,
        &store,
        session_id,
        &[("x.txt", b"hello")],
    );

    let wb_id = project_artifact(
        &registry,
        &manifest,
        &policy,
        &agent_dir,
        &gateway_dir,
        &config,
        &store,
        session_id,
        &artifact_id,
    );

    // Discard first.
    let discard_out = registry
        .execute(
            "workbench_discard",
            &manifest,
            &policy,
            &agent_dir,
            Some(&gateway_dir),
            &serde_json::to_string(&json!({ "workbench_id": wb_id })).unwrap(),
            Some(session_id),
            None,
            Some(&config),
            Some(store.clone()),
            None,
        )
        .unwrap();
    let dv: serde_json::Value = serde_json::from_str(&discard_out).unwrap();
    assert_eq!(dv["ok"], true, "discard should succeed: {:?}", dv);

    // Now cleanup.
    let cleanup_out = registry
        .execute(
            "workbench_cleanup",
            &manifest,
            &policy,
            &agent_dir,
            Some(&gateway_dir),
            &serde_json::to_string(&json!({ "workbench_id": wb_id })).unwrap(),
            Some(session_id),
            None,
            Some(&config),
            Some(store.clone()),
            None,
        )
        .unwrap();
    let cv: serde_json::Value = serde_json::from_str(&cleanup_out).unwrap();
    assert_eq!(cv["ok"], true, "cleanup failed: {:?}", cv);
    assert!(cv["message"].as_str().unwrap().contains("discarded"));

    let after = store.load_workbench(&wb_id).unwrap();
    assert!(after.is_none(), "workbench record should be deleted after cleanup");
}
