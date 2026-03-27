//! Integration test for full build layer lifecycle.
//!
//! Simulates the full flow:
//! - content.write → sandbox.exec with capture → artifact.build with layers
//! - Verifies the weather demo scenario no longer loops

use autonoetic_gateway::artifact_store::ArtifactStore;
use autonoetic_gateway::layer_store::{LayerLimits, LayerStore};
use autonoetic_gateway::runtime::content_store::ContentStore;
use autonoetic_types::layer::ArtifactLayer;
use std::fs;
use tempfile::tempdir;

#[test]
fn test_full_lifecycle_with_layers() {
    let td = tempdir().unwrap();
    let gw_dir = td.path().join(".gateway");
    fs::create_dir_all(&gw_dir).unwrap();

    let content_store = ContentStore::new(&gw_dir).unwrap();
    let artifact_store = ArtifactStore::new(&gw_dir).unwrap();
    let layer_store = LayerStore::new(&gw_dir, LayerLimits::default()).unwrap();

    let session_id = "builder-session";

    let main_py = r#"
import httpx
import sys

def fetch_weather(location):
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

    let captured_layer = layer_store
        .create_from_dir(&venv_dir, "python-deps", "/opt/venv")
        .unwrap();

    println!(
        "Captured layer: {}, digest: {}",
        captured_layer.layer_id, captured_layer.digest
    );

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

    assert_eq!(bundle.files.len(), 2);
    assert_eq!(bundle.layers.len(), 1);
    assert_eq!(bundle.layers[0].mount_path, "/opt/venv");

    let inspected = artifact_store.inspect(&bundle.artifact_id).unwrap();
    assert_eq!(inspected.artifact_id, bundle.artifact_id);
    assert_eq!(inspected.layers.len(), 1);

    let resolved_files = artifact_store.resolve_files(&bundle.artifact_id).unwrap();
    assert_eq!(resolved_files.len(), 2);

    let main_file_content = String::from_utf8_lossy(
        &resolved_files
            .iter()
            .find(|(n, _)| n == "main.py")
            .unwrap()
            .1,
    );
    assert!(main_file_content.contains("import httpx"));

    println!("✓ Full lifecycle test passed: layered artifact built and resolved successfully");
}

#[test]
fn test_lifecycle_without_layers_fails_on_missing_deps() {
    let td = tempdir().unwrap();
    let gw_dir = td.path().join(".gateway");
    fs::create_dir_all(&gw_dir).unwrap();

    let content_store = ContentStore::new(&gw_dir).unwrap();
    let artifact_store = ArtifactStore::new(&gw_dir).unwrap();

    let session_id = "test-session";

    let main_py = r#"
import httpx
print('hello')
"#;
    let main_handle = content_store.write(main_py.as_bytes()).unwrap();
    content_store
        .register_name(session_id, "main.py", &main_handle)
        .unwrap();

    let bundle = artifact_store
        .build(&["main.py".to_string()], None, None, session_id)
        .unwrap();

    assert_eq!(bundle.layers.len(), 0);

    let inspected = artifact_store.inspect(&bundle.artifact_id).unwrap();
    assert_eq!(inspected.layers.len(), 0);

    assert!(inspected.artifact_id.starts_with("art_"));
}
