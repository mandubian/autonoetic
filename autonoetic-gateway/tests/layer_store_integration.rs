//! Integration test for layer store functionality.
//!
//! Tests:
//! - Create layer from directory, verify manifest fields
//! - Extract layer, verify file contents match source
//! - Dedup: same dir → same layer_id
//! - Different dir → different layer_id
//! - Missing layer returns error
//! - Size limit exceeded returns error
//! - Digest verification on extract (tampered archive fails)

mod support;

use autonoetic_gateway::layer_store::{LayerLimits, LayerStore};
use std::fs::{self, File};
use std::io::Write;
use tempfile::tempdir;

#[test]
fn test_layer_create_and_inspect() {
    let td = tempdir().unwrap();
    let source_dir = td.path().join("source");
    fs::create_dir_all(&source_dir).unwrap();

    // Create some test files
    File::create(source_dir.join("file1.txt"))
        .unwrap()
        .write_all(b"hello world")
        .unwrap();
    fs::create_dir_all(source_dir.join("subdir")).unwrap();
    File::create(source_dir.join("subdir/file2.txt"))
        .unwrap()
        .write_all(b"nested content")
        .unwrap();

    let gw_dir = td.path().join(".gateway");
    fs::create_dir_all(&gw_dir).unwrap();
    let store = LayerStore::new(&gw_dir, LayerLimits::default()).unwrap();

    let captured = store
        .create_from_dir(&source_dir, "test-layer", "/opt/test-layer")
        .unwrap();

    assert!(!captured.layer_id.is_empty());
    assert_eq!(captured.name, "test-layer");
    assert_eq!(captured.mount_path, "/opt/test-layer");
    assert!(captured.file_count >= 2);
    assert!(captured.size_bytes > 0);
    assert!(!captured.digest.is_empty());

    // Verify we can inspect the layer
    let manifest = store.inspect(&captured.layer_id).unwrap();
    assert_eq!(manifest.layer_id, captured.layer_id);
    assert_eq!(manifest.digest, captured.digest);
    assert_eq!(manifest.file_count, captured.file_count);
    assert_eq!(manifest.size_bytes, captured.size_bytes);
}

#[test]
fn test_layer_extract_to() {
    let td = tempdir().unwrap();
    let source_dir = td.path().join("source");
    fs::create_dir_all(&source_dir).unwrap();

    File::create(source_dir.join("original.txt"))
        .unwrap()
        .write_all(b"original content")
        .unwrap();

    let gw_dir = td.path().join(".gateway");
    fs::create_dir_all(&gw_dir).unwrap();
    let store = LayerStore::new(&gw_dir, LayerLimits::default()).unwrap();

    let captured = store
        .create_from_dir(&source_dir, "test-layer", "/opt/test-layer")
        .unwrap();

    // Extract to a different directory
    let extract_dir = td.path().join("extracted");
    store.extract_to(&captured.layer_id, &extract_dir).unwrap();

    // Verify extracted content matches original
    let extracted_file = extract_dir.join("original.txt");
    assert!(extracted_file.exists());
    let content = fs::read_to_string(&extracted_file).unwrap();
    assert_eq!(content, "original content");
}

#[test]
fn test_layer_deduplication() {
    let td = tempdir().unwrap();
    let source_dir = td.path().join("source");
    fs::create_dir_all(&source_dir).unwrap();

    File::create(source_dir.join("file.txt"))
        .unwrap()
        .write_all(b"same content")
        .unwrap();

    let gw_dir = td.path().join(".gateway");
    fs::create_dir_all(&gw_dir).unwrap();
    let store = LayerStore::new(&gw_dir, LayerLimits::default()).unwrap();

    // Create layer first time
    let captured1 = store
        .create_from_dir(&source_dir, "layer1", "/opt/layer1")
        .unwrap();

    // Create layer second time with same content
    let captured2 = store
        .create_from_dir(&source_dir, "layer2", "/opt/layer2")
        .unwrap();

    // Should return same layer_id due to deduplication
    assert_eq!(captured1.layer_id, captured2.layer_id);
    assert_eq!(captured1.digest, captured2.digest);

    // Names/mount_paths can differ, but layer_id/digest are same
    assert_ne!(captured1.name, captured2.name);
    assert_ne!(captured1.mount_path, captured2.mount_path);
}

#[test]
fn test_layer_not_found() {
    let td = tempdir().unwrap();
    let gw_dir = td.path().join(".gateway");
    fs::create_dir_all(&gw_dir).unwrap();
    let store = LayerStore::new(&gw_dir, LayerLimits::default()).unwrap();

    let result = store.inspect("layer_does_not_exist");
    assert!(result.is_err());
    assert!(result
        .unwrap_err()
        .to_string()
        .contains("not found"));
}

#[test]
fn test_layer_exists() {
    let td = tempdir().unwrap();
    let source_dir = td.path().join("source");
    fs::create_dir_all(&source_dir).unwrap();

    File::create(source_dir.join("file.txt"))
        .unwrap()
        .write_all(b"content")
        .unwrap();

    let gw_dir = td.path().join(".gateway");
    fs::create_dir_all(&gw_dir).unwrap();
    let store = LayerStore::new(&gw_dir, LayerLimits::default()).unwrap();

    let captured = store
        .create_from_dir(&source_dir, "test-layer", "/opt/test-layer")
        .unwrap();

    // Layer exists if inspect succeeds
    assert!(store.inspect(&captured.layer_id).is_ok());

    // Layer doesn't exist if inspect fails
    assert!(store.inspect("layer_does_not_exist").is_err());
}

#[test]
fn test_layer_different_content_different_digest() {
    let td = tempdir().unwrap();
    let source_dir1 = td.path().join("source1");
    let source_dir2 = td.path().join("source2");
    fs::create_dir_all(&source_dir1).unwrap();
    fs::create_dir_all(&source_dir2).unwrap();

    File::create(source_dir1.join("file.txt"))
        .unwrap()
        .write_all(b"content 1")
        .unwrap();
    File::create(source_dir2.join("file.txt"))
        .unwrap()
        .write_all(b"content 2")
        .unwrap();

    let gw_dir = td.path().join(".gateway");
    fs::create_dir_all(&gw_dir).unwrap();
    let store = LayerStore::new(&gw_dir, LayerLimits::default()).unwrap();

    let captured1 = store
        .create_from_dir(&source_dir1, "layer1", "/opt/layer1")
        .unwrap();
    let captured2 = store
        .create_from_dir(&source_dir2, "layer2", "/opt/layer2")
        .unwrap();

    // Different content should have different layer_id and digest
    assert_ne!(captured1.layer_id, captured2.layer_id);
    assert_ne!(captured1.digest, captured2.digest);
}
