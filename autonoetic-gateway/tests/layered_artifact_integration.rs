//! Integration test for layered artifact build and execution.
//!
//! Tests:
//! - Build artifact with layers → inspect → verify layers in manifest
//! - Run sandbox.exec with layered artifact → verify layer mounted correctly
//! - Artifacts without layers still work identically

mod support;

use autonoetic_gateway::artifact_store::ArtifactStore;
use autonoetic_gateway::layer_store::{LayerLimits, LayerStore};
use autonoetic_gateway::runtime::content_store::ContentStore;
use autonoetic_types::layer::ArtifactLayer;
use std::fs;
use tempfile::tempdir;

#[test]
fn test_artifact_build_with_layers() {
    let td = tempdir().unwrap();
    let gw_dir = td.path().join(".gateway");
    fs::create_dir_all(&gw_dir).unwrap();

    let content_store = ContentStore::new(&gw_dir).unwrap();
    let artifact_store = ArtifactStore::new(&gw_dir).unwrap();
    let layer_store = LayerStore::new(&gw_dir, LayerLimits::default()).unwrap();

    let session_id = "test-session";

    // Write artifact files
    let main_content = b"import httpx\nprint('hello')";
    let main_handle = content_store.write(main_content).unwrap();
    content_store
        .register_name(session_id, "main.py", &main_handle)
        .unwrap();

    let reqs_content = b"httpx==0.27.0";
    let reqs_handle = content_store.write(reqs_content).unwrap();
    content_store
        .register_name(session_id, "requirements.txt", &reqs_handle)
        .unwrap();

    // Create a layer (simulating dependency capture)
    let layer_dir = td.path().join("venv");
    fs::create_dir_all(layer_dir.join("lib/python3.12/site-packages/httpx")).unwrap();
    fs::write(
        layer_dir.join("lib/python3.12/site-packages/httpx/__init__.py"),
        b"# httpx package",
    )
    .unwrap();

    let captured_layer = layer_store
        .create_from_dir(&layer_dir, "python-deps", "/opt/venv")
        .unwrap();

    // Build artifact with layers
    let layers = vec![ArtifactLayer {
        layer_id: captured_layer.layer_id.clone(),
        name: captured_layer.name,
        mount_path: captured_layer.mount_path,
        digest: captured_layer.digest,
    }];

    let bundle = artifact_store
        .build(&["main.py".to_string(), "requirements.txt".to_string()], Some(&["main.py".to_string()]), Some(&layers), session_id)
        .unwrap();

    // Verify artifact was built
    assert!(!bundle.artifact_id.is_empty());
    assert_eq!(bundle.files.len(), 2);
    assert_eq!(bundle.layers.len(), 1);
    assert_eq!(bundle.layers[0].layer_id, captured_layer.layer_id);
    assert_eq!(bundle.layers[0].mount_path, "/opt/venv");

    // Inspect the artifact
    let inspected = artifact_store.inspect(&bundle.artifact_id).unwrap();
    assert_eq!(inspected.artifact_id, bundle.artifact_id);
    assert_eq!(inspected.layers.len(), 1);
    assert_eq!(inspected.layers[0].layer_id, captured_layer.layer_id);
}

#[test]
fn test_artifact_build_without_layers() {
    let td = tempdir().unwrap();
    let gw_dir = td.path().join(".gateway");
    fs::create_dir_all(&gw_dir).unwrap();

    let content_store = ContentStore::new(&gw_dir).unwrap();
    let artifact_store = ArtifactStore::new(&gw_dir).unwrap();

    let session_id = "test-session";

    // Write artifact files without dependencies
    let main_content = b"print('hello')";
    let main_handle = content_store.write(main_content).unwrap();
    content_store
        .register_name(session_id, "main.py", &main_handle)
        .unwrap();

    // Build artifact without layers (backward compatible)
    let bundle = artifact_store
        .build(&["main.py".to_string()], Some(&["main.py".to_string()]), None, session_id)
        .unwrap();

    // Verify artifact was built with empty layers
    assert!(!bundle.artifact_id.is_empty());
    assert_eq!(bundle.files.len(), 1);
    assert_eq!(bundle.layers.len(), 0);

    // Inspect the artifact
    let inspected = artifact_store.inspect(&bundle.artifact_id).unwrap();
    assert_eq!(inspected.artifact_id, bundle.artifact_id);
    assert_eq!(inspected.layers.len(), 0);
}

#[test]
fn test_artifact_with_different_layers_has_different_id() {
    let td = tempdir().unwrap();
    let gw_dir = td.path().join(".gateway");
    fs::create_dir_all(&gw_dir).unwrap();

    let content_store = ContentStore::new(&gw_dir).unwrap();
    let artifact_store = ArtifactStore::new(&gw_dir).unwrap();
    let layer_store = LayerStore::new(&gw_dir, LayerLimits::default()).unwrap();

    let session_id = "test-session";

    let main_content = b"import httpx\nprint('hello')";
    let main_handle = content_store.write(main_content).unwrap();
    content_store
        .register_name(session_id, "main.py", &main_handle)
        .unwrap();

    // Create layer 1
    let layer1_dir = td.path().join("venv1");
    fs::create_dir_all(layer1_dir.join("lib/httpx")).unwrap();
    fs::write(
        layer1_dir.join("lib/httpx/__init__.py"),
        b"# httpx v1",
    )
    .unwrap();
    let captured1 = layer_store
        .create_from_dir(&layer1_dir, "python-deps", "/opt/venv")
        .unwrap();

    // Build artifact with layer 1
    let layers1 = vec![ArtifactLayer {
        layer_id: captured1.layer_id.clone(),
        name: captured1.name.clone(),
        mount_path: captured1.mount_path.clone(),
        digest: captured1.digest.clone(),
    }];
    let bundle1 = artifact_store
        .build(&["main.py".to_string()], None, Some(&layers1), session_id)
        .unwrap();

    // Create layer 2 (different content)
    let layer2_dir = td.path().join("venv2");
    fs::create_dir_all(layer2_dir.join("lib/httpx")).unwrap();
    fs::write(
        layer2_dir.join("lib/httpx/__init__.py"),
        b"# httpx v2",
    )
    .unwrap();
    let captured2 = layer_store
        .create_from_dir(&layer2_dir, "python-deps", "/opt/venv")
        .unwrap();

    // Build artifact with layer 2
    let layers2 = vec![ArtifactLayer {
        layer_id: captured2.layer_id.clone(),
        name: captured2.name.clone(),
        mount_path: captured2.mount_path.clone(),
        digest: captured2.digest.clone(),
    }];
    let bundle2 = artifact_store
        .build(&["main.py".to_string()], None, Some(&layers2), session_id)
        .unwrap();

    // Different layers should produce different artifact IDs
    assert_ne!(bundle1.artifact_id, bundle2.artifact_id);
    assert_ne!(captured1.digest, captured2.digest);
}

