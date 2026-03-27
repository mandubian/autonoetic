//! Layer Store — content-addressed storage for compressed directory trees.
//!
//! Layers are opaque to the gateway. It only tar+compresses, stores, verifies, and extracts.
//!
//! Storage layout:
//! ```text
//! .gateway/layers/
//! ├── index.json                          # digest → layer_id mapping
//! ├── layer_a1b2c3d4/
//! │   ├── manifest.json                   # LayerManifest
//! │   └── contents.tar.zst               # compressed tarball
//! └── layer_e5f6g7h8/
//!     ├── manifest.json
//!     └── contents.tar.zst
//! ```

use autonoetic_types::layer::{ArtifactLayer, CapturedLayer, LayerManifest};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tar::{Archive as TarArchive, Builder as TarBuilder};
use zstd::Encoder as ZstdEncoder;

const LAYER_ID_PREFIX: &str = "layer_";
const LAYERS_DIR: &str = "layers";
const MANIFEST_FILENAME: &str = "manifest.json";
const ARCHIVE_FILENAME: &str = "contents.tar.zst";
const INDEX_FILENAME: &str = "index.json";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LayerLimits {
    pub max_layer_size_bytes: u64,
    pub max_file_count: usize,
}

impl Default for LayerLimits {
    fn default() -> Self {
        Self {
            max_layer_size_bytes: 500 * 1024 * 1024, // 500 MB
            max_file_count: 100_000,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct LayerIndex {
    entries: HashMap<String, String>, // digest → layer_id
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(dead_code)]
struct StoredLayerMeta {
    layer_id: String,
    digest: String,
}

pub struct LayerStore {
    layers_dir: PathBuf,
    index: Arc<Mutex<LayerIndex>>,
    limits: LayerLimits,
}

impl LayerStore {
    pub fn new(gateway_dir: &Path, limits: LayerLimits) -> anyhow::Result<Self> {
        let layers_dir = gateway_dir.join(LAYERS_DIR);
        fs::create_dir_all(&layers_dir)?;
        let index = Self::load_index(&layers_dir)?;
        Ok(Self {
            layers_dir,
            index: Arc::new(Mutex::new(index)),
            limits,
        })
    }

    fn load_index(layers_dir: &Path) -> anyhow::Result<LayerIndex> {
        let index_path = layers_dir.join(INDEX_FILENAME);
        if index_path.exists() {
            let json = fs::read_to_string(&index_path)?;
            Ok(serde_json::from_str(&json)?)
        } else {
            Ok(LayerIndex::default())
        }
    }

    fn save_index(&self, index: &LayerIndex) -> anyhow::Result<()> {
        let index_path = self.layers_dir.join(INDEX_FILENAME);
        let json = serde_json::to_string_pretty(index)?;
        fs::write(&index_path, json)?;
        Ok(())
    }

    fn compute_layer_id(digest: &str) -> String {
        format!(
            "{}{}",
            LAYER_ID_PREFIX,
            &digest[4..12] // first 8 chars after "sha256:"
        )
    }

    #[allow(dead_code)]
    fn archive_path(&self, layer_id: &str) -> PathBuf {
        self.layers_dir.join(layer_id).join(ARCHIVE_FILENAME)
    }

    fn manifest_path(&self, layer_id: &str) -> PathBuf {
        self.layers_dir.join(layer_id).join(MANIFEST_FILENAME)
    }

    pub fn create_from_dir(
        &self,
        source_dir: &Path,
        name: &str,
        mount_path: &str,
    ) -> anyhow::Result<CapturedLayer> {
        let source_dir = source_dir.to_path_buf();
        let name = name.to_string();
        let mount_path = mount_path.to_string();

        // Count files and compute size before archiving
        let mut file_count = 0usize;
        let mut _total_size = 0u64;
        for _entry in walkdir(source_dir.clone())? {
            file_count += 1;
            if file_count > self.limits.max_file_count {
                anyhow::bail!(
                    "layer file count {} exceeds limit {}",
                    file_count,
                    self.limits.max_file_count
                );
            }
        }

        // Create tar + zstd archive in memory
        let mut archive_buffer = Vec::new();
        {
            let encoder = ZstdEncoder::new(&mut archive_buffer, 3)?;
            let mut tar_builder = TarBuilder::new(encoder);
            tar_builder.append_dir_all(".", &source_dir)?;
            let encoder = tar_builder.into_inner()?;
            encoder.finish()?;
        }

        let compressed_size = archive_buffer.len() as u64;
        if compressed_size > self.limits.max_layer_size_bytes {
            anyhow::bail!(
                "layer size {} bytes exceeds limit {} bytes",
                compressed_size,
                self.limits.max_layer_size_bytes
            );
        }

        // Compute digest of the compressed archive
        let mut hasher = Sha256::new();
        hasher.update(&archive_buffer);
        let digest = format!("sha256:{:x}", hasher.finalize());

        // Check for existing layer with same digest (dedup)
        let layer_id = {
            let index = self.index.lock().unwrap();
            if let Some(existing_id) = index.entries.get(&digest) {
                tracing::info!(target: "layer_store", digest = %digest, layer_id = %existing_id, "Reusing existing layer (dedup)");
                return self.captured_from_manifest(existing_id, &name, &mount_path);
            }
            Self::compute_layer_id(&digest)
        };

        // Persist archive
        let layer_dir = self.layers_dir.join(&layer_id);
        fs::create_dir_all(&layer_dir)?;
        let archive_path = layer_dir.join(ARCHIVE_FILENAME);
        fs::write(&archive_path, &archive_buffer)?;

        // Count files and size for manifest
        let (file_count, size_bytes) = Self::count_dir(source_dir)?;

        // Create and persist manifest
        let manifest = LayerManifest {
            layer_id: layer_id.clone(),
            digest: digest.clone(),
            file_count,
            size_bytes,
            created_at: chrono::Utc::now().to_rfc3339(),
        };
        let manifest_path = layer_dir.join(MANIFEST_FILENAME);
        let manifest_json = serde_json::to_string_pretty(&manifest)?;
        fs::write(&manifest_path, manifest_json)?;

        // Update index
        {
            let mut index = self.index.lock().unwrap();
            index.entries.insert(digest.clone(), layer_id.clone());
            self.save_index(&index)?;
        }

        tracing::info!(
            target: "layer_store",
            layer_id = %layer_id,
            digest = %digest,
            file_count = %file_count,
            size_bytes = %size_bytes,
            "Created new layer"
        );

        Ok(CapturedLayer {
            layer_id,
            name,
            mount_path,
            digest,
            file_count,
            size_bytes,
        })
    }

    fn captured_from_manifest(
        &self,
        layer_id: &str,
        name: &str,
        mount_path: &str,
    ) -> anyhow::Result<CapturedLayer> {
        let manifest = self.inspect(layer_id)?;
        Ok(CapturedLayer {
            layer_id: manifest.layer_id,
            name: name.to_string(),
            mount_path: mount_path.to_string(),
            digest: manifest.digest,
            file_count: manifest.file_count,
            size_bytes: manifest.size_bytes,
        })
    }

    fn count_dir(dir: PathBuf) -> anyhow::Result<(usize, u64)> {
        let mut file_count = 0usize;
        let mut size_bytes = 0u64;
        for entry in walkdir(dir.clone())? {
            file_count += 1;
            if let Ok(meta) = entry.metadata() {
                size_bytes += meta.len();
            }
        }
        Ok((file_count, size_bytes))
    }

    pub fn extract_to(&self, layer_id: &str, target_dir: &Path) -> anyhow::Result<()> {
        let manifest = self.inspect(layer_id)?;
        let archive_path = self.layers_dir.join(layer_id).join(ARCHIVE_FILENAME);

        // Verify digest before extraction
        let computed = {
            let file = File::open(&archive_path)?;
            let mut reader = BufReader::new(file);
            let mut hasher = Sha256::new();
            let mut buffer = [0u8; 8192];
            loop {
                let n = reader.read(&mut buffer)?;
                if n == 0 {
                    break;
                }
                hasher.update(&buffer[..n]);
            }
            format!("sha256:{:x}", hasher.finalize())
        };

        if computed != manifest.digest {
            anyhow::bail!(
                "layer '{}' digest mismatch: expected '{}', got '{}'",
                layer_id,
                manifest.digest,
                computed
            );
        }

        // Extract
        let archive_file = File::open(&archive_path)?;
        let reader = BufReader::new(archive_file);
        let decoder = zstd::Decoder::new(reader)?;
        let mut archive = TarArchive::new(decoder);
        archive.unpack(target_dir)?;

        tracing::info!(
            target: "layer_store",
            layer_id = %layer_id,
            target_dir = %target_dir.display(),
            "Layer extracted"
        );

        Ok(())
    }

    pub fn inspect(&self, layer_id: &str) -> anyhow::Result<LayerManifest> {
        let manifest_path = self.manifest_path(layer_id);
        if !manifest_path.exists() {
            anyhow::bail!("layer '{}' not found", layer_id);
        }
        let json = fs::read_to_string(&manifest_path)?;
        let manifest: LayerManifest = serde_json::from_str(&json)?;
        if manifest.layer_id != layer_id {
            anyhow::bail!(
                "layer '{}' manifest has wrong layer_id '{}'",
                layer_id,
                manifest.layer_id
            );
        }
        Ok(manifest)
    }

    pub fn exists_by_digest(&self, digest: &str) -> bool {
        let index = self.index.lock().unwrap();
        index.entries.contains_key(digest)
    }

    pub fn get_by_digest(&self, digest: &str) -> Option<String> {
        let index = self.index.lock().unwrap();
        index.entries.get(digest).cloned()
    }

    pub fn layer_ids_by_digest(&self, digests: &[String]) -> Vec<Option<String>> {
        let index = self.index.lock().unwrap();
        digests
            .iter()
            .map(|d| index.entries.get(d).cloned())
            .collect()
    }

    pub fn resolve_for_artifact(
        &self,
        layers: &[ArtifactLayer],
        temp_base: &Path,
    ) -> anyhow::Result<Vec<(ArtifactLayer, PathBuf)>> {
        let mut result = Vec::new();
        for layer in layers {
            let extract_dir = temp_base.join(&layer.layer_id);
            fs::create_dir_all(&extract_dir)?;
            self.extract_to(&layer.layer_id, &extract_dir)?;
            result.push((layer.clone(), extract_dir));
        }
        Ok(result)
    }
}

fn walkdir(path: PathBuf) -> anyhow::Result<impl Iterator<Item = PathBuf>> {
    let mut entries = Vec::new();
    walkdir_recursive(&path, &mut entries)?;
    Ok(entries.into_iter())
}

fn walkdir_recursive(dir: &Path, entries: &mut Vec<PathBuf>) -> anyhow::Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            walkdir_recursive(&path, entries)?;
        } else {
            entries.push(path);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn create_test_store(temp: &Path) -> LayerStore {
        let gw = temp.join(".gateway");
        fs::create_dir_all(&gw).unwrap();
        LayerStore::new(&gw, LayerLimits::default()).unwrap()
    }

    #[test]
    fn test_create_and_inspect_layer() {
        let temp = tempdir().unwrap();
        let store = create_test_store(temp.path());

        // Create a source directory with files
        let source = temp.path().join("source");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("a.txt"), b"hello").unwrap();
        fs::write(source.join("b.txt"), b"world").unwrap();

        let captured = store
            .create_from_dir(&source, "test-deps", "/tmp/deps")
            .unwrap();

        assert!(captured.layer_id.starts_with("layer_"));
        assert_eq!(captured.name, "test-deps");
        assert_eq!(captured.mount_path, "/tmp/deps");
        assert!(captured.digest.starts_with("sha256:"));
        assert_eq!(captured.file_count, 2);

        // Inspect by layer_id
        let manifest = store.inspect(&captured.layer_id).unwrap();
        assert_eq!(manifest.layer_id, captured.layer_id);
        assert_eq!(manifest.digest, captured.digest);
        assert_eq!(manifest.file_count, 2);

        // exists_by_digest
        assert!(store.exists_by_digest(&captured.digest));
        assert!(!store.exists_by_digest(
            "sha256:0000000000000000000000000000000000000000000000000000000000000000"
        ));
    }

    #[test]
    fn test_layer_dedup() {
        let temp = tempdir().unwrap();
        let store = create_test_store(temp.path());

        let source = temp.path().join("source");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("file.txt"), b"same content").unwrap();

        let captured1 = store.create_from_dir(&source, "deps1", "/tmp/d1").unwrap();
        let captured2 = store.create_from_dir(&source, "deps2", "/tmp/d2").unwrap();

        // Same content → same layer_id
        assert_eq!(captured1.layer_id, captured2.layer_id);
        assert_eq!(captured1.digest, captured2.digest);

        // Only one directory created
        let layer_dir = store.layers_dir.join(&captured1.layer_id);
        assert!(layer_dir.exists());
    }

    #[test]
    fn test_extract_layer() {
        let temp = tempdir().unwrap();
        let store = create_test_store(temp.path());

        let source = temp.path().join("source");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("a.txt"), b"hello").unwrap();
        fs::write(source.join("b.txt"), b"world").unwrap();

        let captured = store.create_from_dir(&source, "test", "/tmp/deps").unwrap();

        let extract_dir = temp.path().join("extract");
        fs::create_dir_all(&extract_dir).unwrap();
        store.extract_to(&captured.layer_id, &extract_dir).unwrap();

        assert_eq!(
            fs::read_to_string(extract_dir.join("a.txt")).unwrap(),
            "hello"
        );
        assert_eq!(
            fs::read_to_string(extract_dir.join("b.txt")).unwrap(),
            "world"
        );
    }

    #[test]
    fn test_digest_verification_on_extract() {
        let temp = tempdir().unwrap();
        let store = create_test_store(temp.path());

        let source = temp.path().join("source");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("file.txt"), b"content").unwrap();

        let captured = store.create_from_dir(&source, "test", "/tmp/deps").unwrap();

        // Tamper with the archive
        let archive_path = store
            .layers_dir
            .join(&captured.layer_id)
            .join(ARCHIVE_FILENAME);
        let tampered = fs::read(&archive_path).unwrap();
        fs::write(&archive_path, &tampered[..tampered.len() - 1]).unwrap();

        let extract_dir = temp.path().join("extract");
        fs::create_dir_all(&extract_dir).unwrap();
        let err = store
            .extract_to(&captured.layer_id, &extract_dir)
            .unwrap_err();
        assert!(err.to_string().contains("digest mismatch"));
    }

    #[test]
    fn test_resolve_for_artifact() {
        let temp = tempdir().unwrap();
        let store = create_test_store(temp.path());

        let source = temp.path().join("source");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("a.txt"), b"content").unwrap();

        let captured = store.create_from_dir(&source, "deps", "/tmp/deps").unwrap();

        let artifact_layers = vec![ArtifactLayer {
            layer_id: captured.layer_id.clone(),
            name: "deps".to_string(),
            mount_path: "/tmp/deps".to_string(),
            digest: captured.digest.clone(),
        }];

        let temp_base = temp.path().join("artifacts");
        fs::create_dir_all(&temp_base).unwrap();
        let resolved = store
            .resolve_for_artifact(&artifact_layers, &temp_base)
            .unwrap();

        assert_eq!(resolved.len(), 1);
        let (_, extract_dir) = &resolved[0];
        assert_eq!(
            fs::read_to_string(extract_dir.join("a.txt")).unwrap(),
            "content"
        );
    }
}
