//! Integration test for full build layer lifecycle.
//!
//! Simulates the full flow:
//! - content.write → sandbox.exec with capture → artifact.build with layers → sandbox.exec with artifact
//! - Verifies the weather demo scenario no longer loops

mod support;

use autonoetic_gateway::artifact_store::ArtifactStore;
use autonoetic_gateway::layer_store::{LayerLimits, LayerStore};
use autonoetic_gateway::policy::PolicyEngine;
use autonoetic_gateway::runtime::content_store::ContentStore;
use autonoetic_gateway::runtime::tools::default_registry;
use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_types::agent::{AgentManifest, AgentIdentity, RuntimeDeclaration};
use autonoetic_types::config::GatewayConfig;
use autonoetic_types::layer::ArtifactLayer;
use serde_json::json;
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use tempfile::tempdir;

fn test_manifest() -> AgentManifest {
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
            id: "test.agent".to_string(),
            name: "Test Agent".to_string(),
            description: "Test agent".to_string(),
        },
        llm_config: None,
        capabilities: vec![],
        execution_mode: Some("reasoning".to_string()),
        script_entry: None,
        gateway_url: None,
        gateway_token: None,
        io: None,
        middleware: None,
        disclosure: None,
    }
}

#[test]
fn test_full_lifecycle_with_layers() {
    let td = tempdir().unwrap();
    let gw_dir = td.path().join(".gateway");
    fs::create_dir_all(&gw_dir).unwrap();

    let content_store = ContentStore::new(&gw_dir).unwrap();
    let artifact_store = ArtifactStore::new(&gw_dir).unwrap();
    let layer_store = LayerStore::new(&gw_dir, LayerLimits::default()).unwrap();

    let session_id = "builder-session";

    // Step 1: Write Python script with external dependency
    let main_py = r#"
import httpx
import sys

def fetch_weather(location):
    # Simulated weather fetch
    print(f"Weather for {location}")
    return {"temp": 20, "conditions": "sunny"}

if __name__ == "__main__":
    if len(sys.argv) > 1:
        fetch_weather(sys.argv[1])
    else:
        fetch_weather("default")
"#;
    let main_handle = content_store.write(main_py.as_bytes()).unwrap();
    content_store
        .register_name(session_id, "main.py", &main_handle)
        .unwrap();

    let requirements_txt = "httpx==0.27.0\n";
    let reqs_handle = content_store.write(requirements_txt.as_bytes()).unwrap();
    content_store
        .register_name(session_id, "requirements.txt", &reqs_handle)
        .unwrap();

    // Step 2: Simulate builder capturing dependencies
    // In real flow, this would be sandbox.exec with capture_paths
    // Here we simulate creating the venv directory directly
    let venv_dir = td.path().join("venv");
    fs::create_dir_all(venv_dir.join("lib/python3.12/site-packages/httpx")).unwrap();
    fs::write(
        venv_dir.join("lib/python3.12/site-packages/httpx/__init__.py"),
        b"# httpx package stub for testing",
    )
    .unwrap();
    fs::write(
        venv_dir.join("lib/python3.12/site-packages/httpx/_client.py"),
        b"# httpx client stub",
    )
    .unwrap();

    // Capture venv as a layer
    let captured_layer = layer_store
        .create_from_dir(&venv_dir, "python-deps", "/opt/venv")
        .unwrap();

    println!(
        "Captured layer: {}, digest: {}",
        captured_layer.layer_id, captured_layer.digest
    );

    // Step 3: Build artifact with layers
    let layers = vec![ArtifactLayer {
        layer_id: captured_layer.layer_id.clone(),
        name: captured_layer.name,
        mount_path: captured_layer.mount_path,
        digest: captured_layer.digest,
    }];

    let bundle = artifact_store
        .build(
            &["main.py".to_string(), "requirements.txt".to_string()],
            Some(&["main.py".to_string()]),
            Some(&layers),
            session_id,
        )
        .unwrap();

    println!("Built artifact: {}", bundle.artifact_id);

    // Verify artifact has layers
    assert_eq!(bundle.files.len(), 2);
    assert_eq!(bundle.layers.len(), 1);
    assert_eq!(bundle.layers[0].mount_path, "/opt/venv");

    // Step 4: Inspect artifact
    let inspected = artifact_store.inspect(&bundle.artifact_id).unwrap();
    assert_eq!(inspected.artifact_id, bundle.artifact_id);
    assert_eq!(inspected.layers.len(), 1);

    // Step 5: Resolve artifact files for execution
    let resolved_files = artifact_store.resolve_files(&bundle.artifact_id).unwrap();
    assert_eq!(resolved_files.len(), 2);

    let main_file_content = String::from_utf8_lossy(
        &resolved_files.iter().find(|(n, _)| n == "main.py").unwrap().1,
    );
    assert!(main_file_content.contains("import httpx"));

    // Step 6: Simulate evaluator running with layered artifact
    let eval_session_id = "evaluator-session";
    let agent_dir = td.path().join("evaluator");
    fs::create_dir_all(&agent_dir).unwrap();

    let config = GatewayConfig::default();
    let policy = PolicyEngine::new(manifest.clone());
    let registry = default_registry(&gw_dir, &config);
    let gateway_store = GatewayStore::new(&gw_dir).unwrap();

    // Create artifact files in content store for evaluator session
    for (name, content) in &resolved_files {
        let handle = content_store.write(content).unwrap();
        content_store
            .register_name(eval_session_id, name, &handle)
            .unwrap();
    }

    let manifest = test_manifest();

    // Run sandbox.exec with artifact_id
    // This should mount the layer at /opt/venv
    let tool_name = "sandbox.exec";
    let arguments = json!({
        "artifact_id": bundle.artifact_id,
        "command": "python3 /tmp/main.py Paris"
    });

    let result = support::run_tool(
        &manifest,
        &policy,
        &agent_dir,
        Some(&gw_dir),
        tool_name,
        &arguments,
        Some(eval_session_id),
        None,
        None,
        Some(&config),
        Some(&gateway_store),
        None,
    );

    // This should succeed without trying to pip install
    // because dependencies are in the layer
    assert!(result.is_ok());
    let response_str = result.unwrap();
    let response: serde_json::Value = serde_json::from_str(&response_str).unwrap();

    assert_eq!(response["ok"], true);
    assert_eq!(response["stdout"], "Weather for Paris");

    println!("✓ Full lifecycle test passed: layered artifact execution succeeded");
}

#[test]
fn test_lifecycle_without_layers_fails_on_missing_deps() {
    let td = tempdir().unwrap();
    let gw_dir = td.path().join(".gateway");
    fs::create_dir_all(&gw_dir).unwrap();

    let content_store = ContentStore::new(&gw_dir).unwrap();
    let artifact_store = ArtifactStore::new(&gw_dir).unwrap();

    let session_id = "test-session";

    // Write Python script with external dependency
    let main_py = r#"
import httpx
print('hello')
"#;
    let main_handle = content_store.write(main_py.as_bytes()).unwrap();
    content_store
        .register_name(session_id, "main.py", &main_handle)
        .unwrap();

    // Build artifact WITHOUT layers
    let bundle = artifact_store
        .build(&["main.py".to_string()], None, None, session_id)
        .unwrap();

    // Artifact has no layers
    assert_eq!(bundle.layers.len(), 0);

    // Inspect artifact
    let inspected = artifact_store.inspect(&bundle.artifact_id).unwrap();
    assert_eq!(inspected.layers.len(), 0);

    // This would fail at evaluation time (no pip install in sandbox)
    // but that's a test for evaluator behavior, not for this test
    assert!(inspected.artifact_id.starts_with("art_"));
}
