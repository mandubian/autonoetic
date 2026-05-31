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
    assert_eq!(list_v["count"], 3);
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
