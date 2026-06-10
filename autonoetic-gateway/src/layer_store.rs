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
        approval_scope: Option<autonoetic_types::layer::LayerApprovalScope>,
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
        let (file_count, size_bytes) = Self::count_dir(source_dir.clone())?;

        // Build-time dependency provenance (read-only, best-effort): record the
        // resolved versions present in the captured tree. The digest already
        // pins the bytes; this makes the closure auditable and blessable.
        let resolved_packages = Self::scan_resolved_packages(&source_dir);

        // Create and persist manifest
        let manifest = LayerManifest {
            layer_id: layer_id.clone(),
            name: name.clone(),
            digest: digest.clone(),
            file_count,
            size_bytes,
            created_at: chrono::Utc::now().to_rfc3339(),
            approval_scope: approval_scope.clone(),
            resolved_packages: resolved_packages.clone(),
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
            approval_scope,
            resolved_packages,
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
            approval_scope: manifest.approval_scope,
            resolved_packages: manifest.resolved_packages,
        })
    }

    /// Scan a captured tree for resolved dependency versions (build-time
    /// provenance). Recursively finds Python `*.dist-info` directories (both
    /// `pip --target` and venv layouts produce these) and parses `name==version`
    /// from the directory stem. Read-only and bounded. (Node `node_modules`
    /// provenance is a follow-up.)
    fn scan_resolved_packages(dir: &Path) -> Vec<autonoetic_types::layer::ResolvedPackage> {
        use autonoetic_types::layer::ResolvedPackage;
        let mut found: Vec<ResolvedPackage> = Vec::new();
        let mut stack = vec![dir.to_path_buf()];
        let mut visited = 0usize;
        while let Some(d) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&d) else {
                continue;
            };
            for entry in entries.flatten() {
                visited += 1;
                if visited > 500_000 {
                    // Don't return silently — truncated provenance must be
                    // detectable so it can't read as "complete" in an audit.
                    tracing::warn!(
                        target: "layer_store",
                        root = %dir.display(),
                        "resolved-package provenance scan hit the entry bound; results may be truncated"
                    );
                    return Self::finalize_resolved(found);
                }
                // `file_type()` does NOT follow symlinks (unlike `path.is_dir()`),
                // so a symlinked dir / cycle can't traverse outside the capture
                // root or leak host package names into the manifest.
                let Ok(file_type) = entry.file_type() else {
                    continue;
                };
                if !file_type.is_dir() {
                    continue;
                }
                let fname = entry.file_name();
                let name = fname.to_string_lossy();
                if let Some(stem) = name.strip_suffix(".dist-info") {
                    if let Some((pkg, ver)) = stem.rsplit_once('-') {
                        if !pkg.is_empty() && !ver.is_empty() {
                            found.push(ResolvedPackage {
                                name: pkg.to_string(),
                                version: ver.to_string(),
                            });
                        }
                    }
                    // Don't descend into the dist-info directory itself.
                    continue;
                }
                stack.push(entry.path());
            }
        }
        Self::finalize_resolved(found)
    }

    fn finalize_resolved(
        mut v: Vec<autonoetic_types::layer::ResolvedPackage>,
    ) -> Vec<autonoetic_types::layer::ResolvedPackage> {
        v.sort_by(|a, b| a.name.cmp(&b.name).then(a.version.cmp(&b.version)));
        v.dedup();
        v
    }

    /// Aggregate resolved-package provenance across a set of layers (e.g. all the
    /// dependency layers in an agent's `runtime.lock`) into one deduplicated,
    /// sorted set — the resolved dependency closure. This is what the approval
    /// boundary surfaces and what bless-on-promotion freezes (determinism inc
    /// 2/3). Missing layers or layers without provenance contribute nothing.
    pub fn aggregate_resolved_packages(
        &self,
        layer_ids: &[String],
    ) -> Vec<autonoetic_types::layer::ResolvedPackage> {
        let mut all = Vec::new();
        for id in layer_ids {
            if let Ok(manifest) = self.inspect(id) {
                all.extend(manifest.resolved_packages);
            }
        }
        Self::finalize_resolved(all)
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
    fn create_from_dir_records_resolved_package_provenance() {
        let temp = tempdir().unwrap();
        let store = create_test_store(temp.path());

        // A captured tree mixing a flat `pip --target` layout and a venv layout.
        let src = temp.path().join("deps");
        fs::create_dir_all(src.join("requests-2.31.0.dist-info")).unwrap();
        fs::create_dir_all(
            src.join("lib/python3.12/site-packages/rich-13.7.0.dist-info"),
        )
        .unwrap();
        // A non-dist-info dir should be ignored (and descended into).
        fs::create_dir_all(src.join("requests")).unwrap();
        fs::write(src.join("requests/__init__.py"), b"").unwrap();

        let captured = store
            .create_from_dir(&src, "python-deps", "/opt/autonoetic-deps", None)
            .unwrap();

        let names: Vec<(String, String)> = captured
            .resolved_packages
            .iter()
            .map(|p| (p.name.clone(), p.version.clone()))
            .collect();
        assert_eq!(
            names,
            vec![
                ("requests".to_string(), "2.31.0".to_string()),
                ("rich".to_string(), "13.7.0".to_string()),
            ],
            "resolved packages parsed from dist-info, sorted by name"
        );

        // Provenance is persisted in the manifest too.
        let manifest = store.inspect(&captured.layer_id).unwrap();
        assert_eq!(manifest.resolved_packages.len(), 2);
    }

    #[test]
    fn aggregate_resolved_packages_merges_and_dedups_across_layers() {
        let temp = tempdir().unwrap();
        let store = create_test_store(temp.path());

        let a = temp.path().join("a");
        fs::create_dir_all(a.join("requests-2.31.0.dist-info")).unwrap();
        let layer_a = store.create_from_dir(&a, "a", "/opt/a", None).unwrap();

        let b = temp.path().join("b");
        fs::create_dir_all(b.join("rich-13.7.0.dist-info")).unwrap();
        // Overlap with layer a — should dedup, not double-count.
        fs::create_dir_all(b.join("requests-2.31.0.dist-info")).unwrap();
        let layer_b = store.create_from_dir(&b, "b", "/opt/b", None).unwrap();

        let merged =
            store.aggregate_resolved_packages(&[layer_a.layer_id, layer_b.layer_id]);
        let pairs: Vec<(String, String)> = merged
            .iter()
            .map(|p| (p.name.clone(), p.version.clone()))
            .collect();
        assert_eq!(
            pairs,
            vec![
                ("requests".to_string(), "2.31.0".to_string()),
                ("rich".to_string(), "13.7.0".to_string()),
            ]
        );

        // Unknown layer ids are skipped silently.
        assert!(store
            .aggregate_resolved_packages(&["nope".to_string()])
            .is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn scan_does_not_follow_symlinked_dirs() {
        let temp = tempdir().unwrap();
        // External dist-info that must NOT be captured via a symlink.
        let external = temp.path().join("external");
        fs::create_dir_all(external.join("evil-1.0.dist-info")).unwrap();
        // Capture root: a real package + a symlink pointing outside.
        let src = temp.path().join("src");
        fs::create_dir_all(src.join("good-2.0.dist-info")).unwrap();
        std::os::unix::fs::symlink(&external, src.join("link")).unwrap();

        let found = LayerStore::scan_resolved_packages(&src);
        let names: Vec<String> = found.iter().map(|p| p.name.clone()).collect();
        assert_eq!(
            names,
            vec!["good".to_string()],
            "symlinked external dist-info must not be traversed"
        );
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
            .create_from_dir(&source, "test-deps", "/tmp/deps", None)
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

        let captured1 = store
            .create_from_dir(&source, "deps1", "/tmp/d1", None)
            .unwrap();
        let captured2 = store
            .create_from_dir(&source, "deps2", "/tmp/d2", None)
            .unwrap();

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

        let captured = store
            .create_from_dir(&source, "test", "/tmp/deps", None)
            .unwrap();

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

        let captured = store
            .create_from_dir(&source, "test", "/tmp/deps", None)
            .unwrap();
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

        let captured = store
            .create_from_dir(&source, "deps", "/tmp/deps", None)
            .unwrap();

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
