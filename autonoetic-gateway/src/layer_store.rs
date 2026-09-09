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

/// One file entry listed from a layer's compressed archive (no extraction).
#[derive(Debug, Clone, Serialize)]
pub struct LayerFileEntry {
    /// Path inside the archive (relative to the layer root).
    pub path: String,
    /// Uncompressed size in bytes.
    pub size: u64,
}

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

/// Where the walk currently is, for `scan_resolved_packages`. npm package roots
/// are identified by their parent, so the role must be carried down the walk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DirRole {
    /// Ordinary directory: check Python / Cargo / Go markers.
    Plain,
    /// A `node_modules/` — its non-dot children are package roots.
    NodeModules,
    /// An `@scope/` inside a `node_modules/` — its children are package roots.
    NpmScope,
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
        let existing_id = {
            let index = self.index.lock().unwrap();
            index.entries.get(&digest).cloned()
        };
        let layer_id = match existing_id {
            Some(existing_id) => {
                tracing::info!(target: "layer_store", digest = %digest, layer_id = %existing_id, "Reusing existing layer (dedup)");
                // Dedup returns before the provenance scan below, so a layer
                // captured by an older scanner keeps its empty package set
                // forever — re-installing the identical tree hits the digest
                // and short-circuits. Backfill so an existing store benefits
                // from a scanner that has learned a new ecosystem, without
                // anyone having to delete layers.
                self.backfill_resolved_packages(&existing_id, &source_dir);
                return self.captured_from_manifest(&existing_id, &name, &mount_path);
            }
            None => Self::compute_layer_id(&digest),
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

    /// Refresh a deduped layer's package provenance when the stored manifest
    /// records none and a scan of the tree now finds some.
    ///
    /// Safe because the digest matched: the tree being scanned is byte-identical
    /// to the archived one, so the result describes the stored layer exactly.
    /// Only the empty case is filled — an existing non-empty set is never
    /// rewritten, so a later capture cannot revise recorded provenance. Scope is
    /// deliberately untouched: `approval_scope` describes the *build session*,
    /// not the content, and legitimately differs between captures of the same
    /// tree.
    ///
    /// Best-effort: a failure here must never fail the capture.
    fn backfill_resolved_packages(&self, layer_id: &str, source_dir: &Path) {
        let Ok(mut manifest) = self.inspect(layer_id) else {
            return;
        };
        if !manifest.resolved_packages.is_empty() {
            return;
        }
        let found = Self::scan_resolved_packages(source_dir);
        if found.is_empty() {
            return;
        }
        let count = found.len();
        manifest.resolved_packages = found;
        let Ok(json) = serde_json::to_string_pretty(&manifest) else {
            return;
        };
        match fs::write(self.manifest_path(layer_id), json) {
            Ok(()) => tracing::info!(
                target: "layer_store",
                layer_id = %layer_id,
                packages = count,
                "Backfilled resolved-package provenance on a deduped layer"
            ),
            Err(e) => tracing::warn!(
                target: "layer_store",
                layer_id = %layer_id,
                error = %e,
                "Failed to backfill resolved-package provenance"
            ),
        }
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
    /// Walk a captured tree and record the packages it resolves, per ecosystem.
    ///
    /// This provenance is what `aggregate_resolved_packages` merges into the
    /// closure the approval boundary surfaces and bless-on-promotion freezes.
    /// It recognised only Python `.dist-info` until now, so every npm, Cargo
    /// and Go layer reported an EMPTY set — all six layers in a real store
    /// (two `node_modules` trees and a Node runtime among them) recorded zero
    /// packages, meaning a Node agent's blessed closure froze nothing and the
    /// operator was shown nothing about what had been installed.
    ///
    /// Layout markers per ecosystem:
    /// - Python: `<name>-<version>.dist-info/`
    /// - npm:    a direct child of a `node_modules/` dir (or of an `@scope/`
    ///           inside one), identified by its own `package.json`
    /// - Cargo:  `<name>-<version>/Cargo.toml` (registry src) or a vendored
    ///           crate dir carrying `.cargo-checksum.json`
    /// - Go:     `<module>@<version>/` in the module cache
    ///
    /// Symlinks are never followed (`file_type()` rather than `path.is_dir()`),
    /// so a symlinked tree cannot pull provenance in from outside the capture.
    fn scan_resolved_packages(dir: &Path) -> Vec<autonoetic_types::layer::ResolvedPackage> {
        use autonoetic_types::layer::ResolvedPackage;
        let mut found: Vec<ResolvedPackage> = Vec::new();
        let mut stack = vec![(dir.to_path_buf(), Self::dir_role(dir))];
        let mut visited = 0usize;
        while let Some((d, role)) = stack.pop() {
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
                // root and contribute provenance that was never captured.
                let Ok(file_type) = entry.file_type() else {
                    continue;
                };
                if !file_type.is_dir() {
                    continue;
                }
                let fname = entry.file_name();
                let name = fname.to_string_lossy();
                let path = entry.path();

                match role {
                    DirRole::NodeModules => {
                        // `.bin`, `.package-lock.json` and friends are not packages.
                        if name.starts_with('.') {
                            continue;
                        }
                        if name.starts_with('@') {
                            stack.push((path, DirRole::NpmScope));
                            continue;
                        }
                        if let Some(pkg) = Self::read_npm_package(&path) {
                            found.push(pkg);
                        }
                        // Descend anyway: packages can nest their own node_modules.
                        stack.push((path, DirRole::Plain));
                        continue;
                    }
                    DirRole::NpmScope => {
                        if let Some(pkg) = Self::read_npm_package(&path) {
                            found.push(pkg);
                        }
                        stack.push((path, DirRole::Plain));
                        continue;
                    }
                    DirRole::Plain => {}
                }

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

                // Go module cache: `<module>@<version>`. Guarded against index 0
                // so an npm scope dir reached out of role can never match here.
                if let Some(at) = name.find('@') {
                    if at > 0 {
                        let (module, version) = name.split_at(at);
                        let version = &version[1..];
                        if !module.is_empty() && !version.is_empty() {
                            found.push(ResolvedPackage {
                                name: module.to_string(),
                                version: version.to_string(),
                            });
                            stack.push((path, DirRole::Plain));
                            continue;
                        }
                    }
                }

                if let Some(pkg) = Self::read_cargo_package(&path, &name) {
                    found.push(pkg);
                    continue;
                }

                let role = Self::dir_role(&path);
                stack.push((path, role));
            }
        }
        Self::finalize_resolved(found)
    }

    /// A directory's role in the walk — npm package roots are identified by
    /// their PARENT being `node_modules` (or a scope inside one), not by any
    /// marker of their own, so the role has to be carried down the walk.
    fn dir_role(path: &Path) -> DirRole {
        match path.file_name().and_then(|n| n.to_str()) {
            Some("node_modules") => DirRole::NodeModules,
            _ => DirRole::Plain,
        }
    }

    /// Read `<dir>/package.json` and return its declared name + version.
    /// The manifest's own `name` wins over the directory name so a scoped
    /// package records as `@scope/pkg`, matching how it is required.
    fn read_npm_package(dir: &Path) -> Option<autonoetic_types::layer::ResolvedPackage> {
        let raw = std::fs::read_to_string(dir.join("package.json")).ok()?;
        let v: serde_json::Value = serde_json::from_str(&raw).ok()?;
        let name = v.get("name")?.as_str()?.trim().to_string();
        let version = v.get("version")?.as_str()?.trim().to_string();
        if name.is_empty() || version.is_empty() {
            return None;
        }
        Some(autonoetic_types::layer::ResolvedPackage { name, version })
    }

    /// Recognise a Cargo crate directory: registry `src/<name>-<version>/` or a
    /// vendored crate carrying `.cargo-checksum.json`. The version comes from
    /// `Cargo.toml` when readable (vendored dirs are named without one) and
    /// falls back to the `<name>-<version>` directory name.
    fn read_cargo_package(
        dir: &Path,
        dir_name: &str,
    ) -> Option<autonoetic_types::layer::ResolvedPackage> {
        let has_manifest = dir.join("Cargo.toml").is_file();
        if !has_manifest && !dir.join(".cargo-checksum.json").is_file() {
            return None;
        }
        if let Ok(raw) = std::fs::read_to_string(dir.join("Cargo.toml")) {
            if let Ok(parsed) = raw.parse::<toml::Value>() {
                let pkg = parsed.get("package");
                let name = pkg.and_then(|p| p.get("name")).and_then(|n| n.as_str());
                let version = pkg.and_then(|p| p.get("version")).and_then(|n| n.as_str());
                if let (Some(name), Some(version)) = (name, version) {
                    if !name.is_empty() && !version.is_empty() {
                        return Some(autonoetic_types::layer::ResolvedPackage {
                            name: name.to_string(),
                            version: version.to_string(),
                        });
                    }
                }
            }
        }
        // A workspace manifest inherits its version and names no package —
        // fall back to the registry-src directory name.
        let (name, version) = dir_name.rsplit_once('-')?;
        if name.is_empty() || version.is_empty() || !version.starts_with(|c: char| c.is_ascii_digit())
        {
            return None;
        }
        Some(autonoetic_types::layer::ResolvedPackage {
            name: name.to_string(),
            version: version.to_string(),
        })
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

    /// Stream the file entries from a layer's `contents.tar.zst` **without
    /// extracting** it. Returns the (capped) entries, the total non-directory
    /// file count, and whether the listing was truncated at `limit`.
    ///
    /// Directory entries (paths ending in `/`) are skipped. The total counts
    /// every non-directory entry in the archive so `truncated` is accurate
    /// even when the caller's limit cuts the listing short.
    pub fn list_files(
        &self,
        layer_id: &str,
        limit: usize,
    ) -> anyhow::Result<(Vec<LayerFileEntry>, usize, bool)> {
        let archive_path = self.layers_dir.join(layer_id).join(ARCHIVE_FILENAME);
        if !archive_path.exists() {
            anyhow::bail!("layer '{}' archive not found", layer_id);
        }
        let file = File::open(&archive_path)?;
        let reader = BufReader::new(file);
        let decoder = zstd::Decoder::new(reader)?;
        let mut archive = TarArchive::new(decoder);

        let mut entries_out = Vec::new();
        let mut total = 0usize;
        let mut truncated = false;
        for entry in archive.entries()? {
            let mut entry = entry?;
            // Skip directory entries — tar marks them with a trailing `/`.
            if entry.header().entry_type().is_dir() {
                continue;
            }
            total += 1;
            if entries_out.len() < limit {
                let path = entry.path()?.to_string_lossy().to_string();
                let size = entry.header().size().unwrap_or(0);
                // Drain so the iterator can advance without reading the body.
                let _ = entry.read_to_end(&mut Vec::new());
                entries_out.push(LayerFileEntry { path, size });
            } else {
                truncated = true;
            }
        }
        Ok((entries_out, total, truncated))
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

    fn write_npm_pkg(root: &std::path::Path, rel: &str, name: &str, version: &str) {
        let dir = root.join(rel);
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("package.json"),
            format!(r#"{{"name":"{name}","version":"{version}"}}"#),
        )
        .unwrap();
    }

    /// The defect this scanner had: npm layers recorded ZERO packages, so a
    /// Node agent's blessed closure froze nothing. Real store evidence: two
    /// `node_modules` trees and a Node runtime, all `resolved_packages=0`.
    #[test]
    fn npm_node_modules_resolve_including_scoped_and_nested() {
        let temp = tempdir().unwrap();
        let src = temp.path().join("node_modules");
        fs::create_dir_all(&src).unwrap();
        write_npm_pkg(&src, "agent-browser", "agent-browser", "0.27.0");
        write_npm_pkg(&src, "@scope/inner", "@scope/inner", "2.1.0");
        // A package nesting its own dependency.
        write_npm_pkg(&src, "outer", "outer", "1.0.0");
        write_npm_pkg(&src, "outer/node_modules/nested", "nested", "3.3.3");
        // `.bin` shims are not packages.
        fs::create_dir_all(src.join(".bin")).unwrap();

        let found = LayerStore::scan_resolved_packages(&src);
        let got: Vec<String> = found
            .iter()
            .map(|p| format!("{}@{}", p.name, p.version))
            .collect();
        assert_eq!(
            got,
            vec![
                "@scope/inner@2.1.0".to_string(),
                "agent-browser@0.27.0".to_string(),
                "nested@3.3.3".to_string(),
                "outer@1.0.0".to_string(),
            ],
            "npm packages (scoped + nested) must resolve; .bin must not"
        );
    }

    #[test]
    fn cargo_registry_and_vendored_crates_resolve() {
        let temp = tempdir().unwrap();
        let src = temp.path().join("cargo");
        // Registry src layout: <name>-<version>/Cargo.toml
        let reg = src.join("registry/src/index.crates.io-abc/serde-1.0.219");
        fs::create_dir_all(&reg).unwrap();
        fs::write(
            reg.join("Cargo.toml"),
            "[package]\nname = \"serde\"\nversion = \"1.0.219\"\n",
        )
        .unwrap();
        // Vendored crate: dir name carries no version, manifest does.
        let vend = src.join("vendor/anyhow");
        fs::create_dir_all(&vend).unwrap();
        fs::write(vend.join(".cargo-checksum.json"), "{}").unwrap();
        fs::write(
            vend.join("Cargo.toml"),
            "[package]\nname = \"anyhow\"\nversion = \"1.0.95\"\n",
        )
        .unwrap();

        let found = LayerStore::scan_resolved_packages(&src);
        let got: Vec<String> = found
            .iter()
            .map(|p| format!("{}@{}", p.name, p.version))
            .collect();
        assert_eq!(got, vec!["anyhow@1.0.95", "serde@1.0.219"]);
    }

    #[test]
    fn go_module_cache_entries_resolve() {
        let temp = tempdir().unwrap();
        let src = temp.path().join("pkg/mod");
        fs::create_dir_all(src.join("github.com/pkg/errors@v0.9.1")).unwrap();
        fs::create_dir_all(src.join("golang.org/x/net@v0.33.0")).unwrap();

        let found = LayerStore::scan_resolved_packages(&src);
        let got: Vec<String> = found
            .iter()
            .map(|p| format!("{}@{}", p.name, p.version))
            .collect();
        assert_eq!(got, vec!["errors@v0.9.1", "net@v0.33.0"]);
    }

    /// A project's own manifest is not a resolved dependency — only children of
    /// a `node_modules/` are. Without this the scanner would claim the agent's
    /// own package as part of its dependency closure.
    #[test]
    fn a_bare_project_package_json_is_not_a_resolved_package() {
        let temp = tempdir().unwrap();
        let src = temp.path().join("proj");
        fs::create_dir_all(&src).unwrap();
        fs::write(
            src.join("package.json"),
            r#"{"name":"my-app","version":"0.1.0"}"#,
        )
        .unwrap();
        let found = LayerStore::scan_resolved_packages(&src);
        assert!(found.is_empty(), "project manifest must not resolve: {found:?}");
    }

    #[test]
    fn python_dist_info_still_resolves_alongside_the_new_ecosystems() {
        let temp = tempdir().unwrap();
        let src = temp.path().join("mixed");
        fs::create_dir_all(src.join("lib/python3.11/site-packages/requests-2.32.3.dist-info"))
            .unwrap();
        write_npm_pkg(&src, "node_modules/left-pad", "left-pad", "1.3.0");
        let found = LayerStore::scan_resolved_packages(&src);
        let got: Vec<String> = found
            .iter()
            .map(|p| format!("{}@{}", p.name, p.version))
            .collect();
        assert_eq!(got, vec!["left-pad@1.3.0", "requests@2.32.3"]);
    }

    /// Two layers carrying different versions of the same package both survive
    /// aggregation — dedup is on the (name, version) pair. That is the signal
    /// a conflict check needs, and it is only observable now that non-Python
    /// ecosystems resolve at all.
    #[test]
    fn conflicting_versions_across_layers_both_appear_in_the_aggregate() {
        let temp = tempdir().unwrap();
        let store = create_test_store(temp.path());
        let a = temp.path().join("a/node_modules");
        let b = temp.path().join("b/node_modules");
        fs::create_dir_all(&a).unwrap();
        fs::create_dir_all(&b).unwrap();
        write_npm_pkg(&a, "lodash", "lodash", "4.17.21");
        write_npm_pkg(&b, "lodash", "lodash", "3.10.1");
        let la = store
            .create_from_dir(&a, "a", "/tmp/a/node_modules", None)
            .unwrap();
        let lb = store
            .create_from_dir(&b, "b", "/tmp/b/node_modules", None)
            .unwrap();

        let agg = store.aggregate_resolved_packages(&[la.layer_id, lb.layer_id]);
        let got: Vec<String> = agg
            .iter()
            .map(|p| format!("{}@{}", p.name, p.version))
            .collect();
        assert_eq!(
            got,
            vec!["lodash@3.10.1", "lodash@4.17.21"],
            "both versions must be visible — the sandbox can only reach one"
        );
    }

    /// The scenario this exists for: a layer captured before the scanner knew
    /// npm keeps an empty package set forever, because re-capturing the same
    /// tree hits the digest and returns before the scan.
    #[test]
    fn dedup_backfills_provenance_a_stale_manifest_is_missing() {
        let temp = tempdir().unwrap();
        let store = create_test_store(temp.path());
        let src = temp.path().join("node_modules");
        fs::create_dir_all(&src).unwrap();
        write_npm_pkg(&src, "left-pad", "left-pad", "1.3.0");

        let captured = store
            .create_from_dir(&src, "deps", "/tmp/node_modules", None)
            .unwrap();
        // Simulate a manifest written by the old Python-only scanner.
        let mut manifest = store.inspect(&captured.layer_id).unwrap();
        manifest.resolved_packages.clear();
        fs::write(
            store.manifest_path(&captured.layer_id),
            serde_json::to_string_pretty(&manifest).unwrap(),
        )
        .unwrap();
        assert!(store.inspect(&captured.layer_id).unwrap().resolved_packages.is_empty());

        // Re-capturing the identical tree dedups — and now backfills.
        let again = store
            .create_from_dir(&src, "deps", "/tmp/node_modules", None)
            .unwrap();
        assert_eq!(again.layer_id, captured.layer_id, "still deduped");
        let refreshed = store.inspect(&captured.layer_id).unwrap();
        assert_eq!(
            refreshed
                .resolved_packages
                .iter()
                .map(|p| format!("{}@{}", p.name, p.version))
                .collect::<Vec<_>>(),
            vec!["left-pad@1.3.0"],
            "provenance backfilled without deleting the layer"
        );
        assert_eq!(refreshed.digest, captured.digest, "digest untouched");
    }

    /// Recorded provenance is never revised — only the empty case is filled.
    #[test]
    fn dedup_never_overwrites_existing_provenance() {
        let temp = tempdir().unwrap();
        let store = create_test_store(temp.path());
        let src = temp.path().join("node_modules");
        fs::create_dir_all(&src).unwrap();
        write_npm_pkg(&src, "left-pad", "left-pad", "1.3.0");
        let captured = store
            .create_from_dir(&src, "deps", "/tmp/node_modules", None)
            .unwrap();

        // Pin a deliberately different recorded set.
        let mut manifest = store.inspect(&captured.layer_id).unwrap();
        manifest.resolved_packages = vec![autonoetic_types::layer::ResolvedPackage {
            name: "recorded-at-capture".to_string(),
            version: "9.9.9".to_string(),
        }];
        fs::write(
            store.manifest_path(&captured.layer_id),
            serde_json::to_string_pretty(&manifest).unwrap(),
        )
        .unwrap();

        store
            .create_from_dir(&src, "deps", "/tmp/node_modules", None)
            .unwrap();
        let after = store.inspect(&captured.layer_id).unwrap();
        assert_eq!(
            after.resolved_packages.len(),
            1,
            "an existing set must not be revised by a later capture"
        );
        assert_eq!(after.resolved_packages[0].name, "recorded-at-capture");
    }

    /// A layer with genuinely nothing to resolve (compiled output, conda) must
    /// not be rewritten on every dedup hit.
    #[test]
    fn dedup_leaves_a_legitimately_empty_layer_alone() {
        let temp = tempdir().unwrap();
        let store = create_test_store(temp.path());
        let src = temp.path().join("compiled");
        fs::create_dir_all(src.join("bin")).unwrap();
        fs::write(src.join("bin").join("tool"), b"ELF").unwrap();
        let captured = store
            .create_from_dir(&src, "compiled", "/tmp/compiled", None)
            .unwrap();
        let before = fs::metadata(store.manifest_path(&captured.layer_id))
            .unwrap()
            .modified()
            .unwrap();

        store
            .create_from_dir(&src, "compiled", "/tmp/compiled", None)
            .unwrap();
        let after_manifest = store.inspect(&captured.layer_id).unwrap();
        assert!(after_manifest.resolved_packages.is_empty());
        let after = fs::metadata(store.manifest_path(&captured.layer_id))
            .unwrap()
            .modified()
            .unwrap();
        assert_eq!(before, after, "manifest must not be rewritten with nothing to add");
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

    #[test]
    fn test_list_files_streams_archive_without_extraction() {
        let temp = tempdir().unwrap();
        let store = create_test_store(temp.path());

        // A source tree with nested directories and files.
        let source = temp.path().join("source");
        fs::create_dir_all(source.join("lib/httpx")).unwrap();
        fs::write(source.join("lib/httpx/__init__.py"), b"# httpx").unwrap();
        fs::write(source.join("lib/httpx/client.py"), b"# client").unwrap();
        fs::write(source.join("main.py"), b"print('hi')").unwrap();

        let captured = store
            .create_from_dir(&source, "python-deps", "/opt/venv", None)
            .unwrap();

        // list_files streams the archive — directories are skipped, only
        // regular files appear, with their uncompressed sizes.
        let (entries, total, truncated) = store.list_files(&captured.layer_id, 500).unwrap();
        assert_eq!(total, 3);
        assert!(!truncated);
        assert_eq!(entries.len(), 3);

        let paths: Vec<&str> = entries.iter().map(|e| e.path.as_str()).collect();
        // All three files present (order is archive order, not sorted).
        assert!(paths.iter().any(|p| p.ends_with("main.py")));
        assert!(paths.iter().any(|p| p.ends_with("__init__.py")));
        assert!(paths.iter().any(|p| p.ends_with("client.py")));
        // Sizes match uncompressed content lengths.
        let main = entries.iter().find(|e| e.path.ends_with("main.py")).unwrap();
        assert_eq!(main.size, b"print('hi')".len() as u64);
    }

    #[test]
    fn test_list_files_respects_limit_and_reports_truncation() {
        let temp = tempdir().unwrap();
        let store = create_test_store(temp.path());

        let source = temp.path().join("source");
        fs::create_dir_all(&source).unwrap();
        for i in 0..5 {
            fs::write(source.join(format!("f{i}.txt")), b"x").unwrap();
        }

        let captured = store
            .create_from_dir(&source, "deps", "/tmp/d", None)
            .unwrap();

        // Limit below the file count: entries are capped, total is accurate,
        // and truncated is flagged.
        let (entries, total, truncated) = store.list_files(&captured.layer_id, 2).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(total, 5);
        assert!(truncated);
    }

    #[test]
    fn test_list_files_missing_layer_errors() {
        let temp = tempdir().unwrap();
        let store = create_test_store(temp.path());

        let err = store.list_files("layer_nope", 10).unwrap_err();
        assert!(err.to_string().contains("not found"));
    }
}
