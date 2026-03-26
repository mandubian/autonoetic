//! Artifact Store — immutable file bundles for review/install/execution.
//!
//! Artifacts are the only units that may cross trust boundaries.
//! Built from session content and immutable once created.
//!
//! Storage layout:
//! ```text
//! .gateway/artifacts/
//! ├── index.json                      # artifact_id → manifest path mapping
//! ├── art_a1b2c3d4/
//! │   └── manifest.json
//! └── art_e5f6g7h8/
//!     └── manifest.json
//! ```

use crate::runtime::content_store::{root_session_id, ContentStore};
use autonoetic_types::artifact::{ArtifactBundle, ArtifactFileEntry};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Short artifact ID prefix.
const ARTIFACT_ID_PREFIX: &str = "art_";

/// Index mapping artifact_id → artifact directory name.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default)]
struct ArtifactIndex {
    entries: HashMap<String, String>,
}

/// Store for immutable artifact bundles.
pub struct ArtifactStore {
    /// Root path for gateway-owned session and artifact projections.
    gateway_dir: PathBuf,
    /// Root path for artifact storage (.gateway/artifacts/)
    artifacts_dir: PathBuf,
    /// Reference to the content store for resolving file handles
    content_store: ContentStore,
}

impl ArtifactStore {
    /// Creates a new ArtifactStore.
    pub fn new(gateway_dir: &Path) -> anyhow::Result<Self> {
        let artifacts_dir = gateway_dir.join("artifacts");
        std::fs::create_dir_all(&artifacts_dir)?;
        let content_store = ContentStore::new(gateway_dir)?;
        Ok(Self {
            gateway_dir: gateway_dir.to_path_buf(),
            artifacts_dir,
            content_store,
        })
    }

    /// Computes a deterministic artifact ID from sorted inputs and entrypoints.
    /// Same inputs + entrypoints always produce the same artifact ID.
    fn compute_deterministic_artifact_id(
        file_handles: &[String],
        entrypoints: Option<&[String]>,
    ) -> String {
        let mut hasher = Sha256::new();

        // Sort file handles for determinism
        let mut sorted_handles = file_handles.to_vec();
        sorted_handles.sort();
        for handle in sorted_handles {
            hasher.update(handle.as_bytes());
            hasher.update(b"\0"); // Separator
        }

        // Sort entrypoints for determinism
        if let Some(eps) = entrypoints {
            let mut sorted_eps = eps.to_vec();
            sorted_eps.sort();
            for ep in sorted_eps {
                hasher.update(ep.as_bytes());
                hasher.update(b"\0");
            }
        }

        let hash = hasher.finalize();
        // Use first 8 hex chars for short ID (same length as UUID-based)
        format!(
            "{}{:08x}",
            ARTIFACT_ID_PREFIX,
            u32::from_be_bytes([hash[0], hash[1], hash[2], hash[3]])
        )
    }

    /// Checks if an artifact with the given ID exists.
    fn artifact_exists(&self, artifact_id: &str) -> bool {
        self.artifacts_dir
            .join(artifact_id)
            .join("manifest.json")
            .exists()
    }

    /// Computes SHA-256 digest of the manifest JSON.
    fn compute_digest(bundle_json: &str) -> String {
        let mut hasher = Sha256::new();
        hasher.update(bundle_json.as_bytes());
        format!("sha256:{:x}", hasher.finalize())
    }
    
    /// Recomputes the canonical digest for a persisted artifact bundle.
    ///
    /// The digest is defined over the manifest bytes with the `digest` field blanked,
    /// matching the build-time computation.
    fn compute_bundle_digest(bundle: &ArtifactBundle) -> anyhow::Result<String> {
        let mut canonical = bundle.clone();
        canonical.digest.clear();
        let bundle_json = serde_json::to_string(&canonical)?;
        Ok(Self::compute_digest(&bundle_json))
    }

    /// Sorted (name, handle) pairs and sorted entrypoints for identity checks on dedup.
    fn normalized_artifact_identity(
        files: &[ArtifactFileEntry],
        entrypoints: &[String],
    ) -> (Vec<(String, String)>, Vec<String>) {
        let mut pairs: Vec<(String, String)> = files
            .iter()
            .map(|f| (f.name.clone(), f.handle.clone()))
            .collect();
        pairs.sort();
        let mut eps = entrypoints.to_vec();
        eps.sort();
        (pairs, eps)
    }

    /// Loads the artifact index from disk.
    fn load_index(&self) -> anyhow::Result<ArtifactIndex> {
        let index_path = self.artifacts_dir.join("index.json");
        if index_path.exists() {
            let json = std::fs::read_to_string(&index_path)?;
            Ok(serde_json::from_str(&json)?)
        } else {
            Ok(ArtifactIndex::default())
        }
    }

    /// Saves the artifact index to disk.
    fn save_index(&self, index: &ArtifactIndex) -> anyhow::Result<()> {
        let index_path = self.artifacts_dir.join("index.json");
        let json = serde_json::to_string_pretty(index)?;
        std::fs::write(&index_path, json)?;
        Ok(())
    }

    /// Builds an artifact from session-visible content.
    ///
    /// With deduplication: if an artifact with the same inputs + entrypoints already exists,
    /// returns the existing artifact (with `reused: true`). Otherwise creates a new artifact.
    ///
    /// - `inputs`: list of content names or handles to include
    /// - `entrypoints`: optional list of entrypoint filenames
    /// - `builder_session_id`: session that is building this artifact
    pub fn build(
        &self,
        inputs: &[String],
        entrypoints: Option<&[String]>,
        builder_session_id: &str,
    ) -> anyhow::Result<ArtifactBundle> {
        anyhow::ensure!(!inputs.is_empty(), "artifact inputs must not be empty");

        let mut files = Vec::new();
        let mut file_handles = Vec::new();

        // Phase 1: Resolve all inputs to file handles
        for input_name in inputs {
            // Resolve content: try as name first, then as handle (with visibility check)
            let (handle, content) = if input_name.starts_with("sha256:") {
                // Handle inputs must also be visible — handles are not bearer tokens
                if !self
                    .content_store
                    .is_handle_visible(builder_session_id, input_name)?
                {
                    anyhow::bail!(
                        "Content handle '{}' is not visible in session '{}' or its root session",
                        input_name,
                        builder_session_id
                    );
                }
                let content = self.content_store.read(&input_name.to_string())?;
                (input_name.clone(), content)
            } else {
                let handle = self
                    .content_store
                    .resolve_name_with_root(builder_session_id, input_name)?;
                let content = self.content_store.read(&handle)?;
                (handle, content)
            };

            let alias = ContentStore::get_short_alias(&handle);

            // Verify content is non-empty
            anyhow::ensure!(
                !content.is_empty(),
                "artifact input '{}' resolved to empty content",
                input_name
            );

            file_handles.push(handle.clone());
            files.push(ArtifactFileEntry {
                name: input_name.clone(),
                handle,
                alias,
            });
        }

        // Validate entrypoints before dedup so we never "reuse" with invalid args.
        let ep: Vec<String> = if let Some(eps) = entrypoints {
            for e in eps {
                anyhow::ensure!(
                    files.iter().any(|f| f.name == *e),
                    "entrypoint '{}' not found in artifact inputs",
                    e
                );
            }
            eps.to_vec()
        } else {
            Vec::new()
        };

        // Phase 2: Compute deterministic artifact ID from handles + entrypoints
        let artifact_id =
            Self::compute_deterministic_artifact_id(&file_handles, Some(ep.as_slice()));

        // Phase 3: Check if artifact already exists (deduplication)
        if self.artifact_exists(&artifact_id) {
            let existing_bundle = self.inspect(&artifact_id)?;
            let (want_pairs, want_eps) = Self::normalized_artifact_identity(&files, &ep);
            let (got_pairs, got_eps) = Self::normalized_artifact_identity(
                &existing_bundle.files,
                &existing_bundle.entrypoints,
            );
            if want_pairs != got_pairs || want_eps != got_eps {
                anyhow::bail!(
                    "artifact id '{}' already exists but its manifest does not match the requested inputs (identity mismatch). Refusing reuse; remove or repair the on-disk artifact if it is corrupted.",
                    artifact_id
                );
            }
            tracing::info!(
                target: "artifact_store",
                artifact_id = %artifact_id,
                "Reused existing artifact (deduplication)"
            );
            return Ok(ArtifactBundle {
                reused: true,
                ..existing_bundle
            });
        }

        // Phase 4: Create new artifact
        let created_at = chrono::Utc::now().to_rfc3339();

        let bundle = ArtifactBundle {
            artifact_id: artifact_id.clone(),
            files,
            entrypoints: ep,
            digest: String::new(), // computed below
            created_at,
            builder_session_id: builder_session_id.to_string(),
            reused: false,
        };

        // Compute digest from canonical JSON
        let bundle_json = serde_json::to_string(&bundle)?;
        let digest = Self::compute_digest(&bundle_json);

        let bundle = ArtifactBundle { digest, ..bundle };

        // Persist to disk
        self.persist_bundle(&bundle)?;

        tracing::info!(
            target: "artifact_store",
            artifact_id = %bundle.artifact_id,
            file_count = bundle.files.len(),
            "Built new artifact"
        );

        Ok(bundle)
    }

    /// Persists an artifact bundle to disk.
    fn persist_bundle(&self, bundle: &ArtifactBundle) -> anyhow::Result<()> {
        let dir = self.artifacts_dir.join(&bundle.artifact_id);
        std::fs::create_dir_all(&dir)?;

        let manifest_path = dir.join("manifest.json");
        let json = serde_json::to_string_pretty(bundle)?;
        std::fs::write(&manifest_path, json)?;
        let mut perms = std::fs::metadata(&manifest_path)?.permissions();
        perms.set_readonly(true);
        std::fs::set_permissions(&manifest_path, perms)?;

        // Update index
        let mut index = self.load_index()?;
        index
            .entries
            .insert(bundle.artifact_id.clone(), bundle.artifact_id.clone());
        self.save_index(&index)?;
        self.materialize_session_projection(bundle)?;

        Ok(())
    }

    /// Creates a human-readable, session-local projection of the artifact files.
    ///
    /// This keeps the content- and artifact-addressed stores canonical while giving
    /// operators a stable path like:
    /// `.gateway/sessions/<session>/artifacts/<artifact_id>/<file>`
    fn materialize_session_projection(&self, bundle: &ArtifactBundle) -> anyhow::Result<()> {
        let base_session_id = root_session_id(&bundle.builder_session_id);
        let artifact_dir = self
            .gateway_dir
            .join("sessions")
            .join(base_session_id)
            .join("artifacts")
            .join(&bundle.artifact_id);
        std::fs::create_dir_all(&artifact_dir)?;

        for file in &bundle.files {
            let output_path = artifact_dir.join(&file.name);
            if let Some(parent) = output_path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            self.materialize_projection_file(&file.handle, &output_path)?;
        }

        std::fs::write(
            artifact_dir.join("README.md"),
            self.render_session_projection_readme(bundle),
        )?;

        Ok(())
    }

    fn materialize_projection_file(&self, handle: &str, output_path: &Path) -> anyhow::Result<()> {
        if output_path.exists() || output_path.is_symlink() {
            std::fs::remove_file(output_path)?;
        }

        let blob_path = self.content_store.blob_path(&handle.to_string());

        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            symlink(&blob_path, output_path)?;
            return Ok(());
        }

        #[cfg(not(unix))]
        {
            if std::fs::hard_link(&blob_path, output_path).is_ok() {
                return Ok(());
            }
            let content = self.content_store.read(&handle.to_string())?;
            std::fs::write(output_path, content)?;
            Ok(())
        }
    }

    fn render_session_projection_readme(&self, bundle: &ArtifactBundle) -> String {
        let mut lines = vec![
            format!("# Artifact `{}`", bundle.artifact_id),
            String::new(),
            format!("- Digest: `{}`", bundle.digest),
            format!("- Created At: `{}`", bundle.created_at),
            format!("- Builder Session: `{}`", bundle.builder_session_id),
            String::new(),
            "## Entrypoints".to_string(),
        ];

        if bundle.entrypoints.is_empty() {
            lines.push("- None".to_string());
        } else {
            for entrypoint in &bundle.entrypoints {
                lines.push(format!("- `{}`", entrypoint));
            }
        }

        lines.push(String::new());
        lines.push("## Files".to_string());
        for file in &bundle.files {
            lines.push(format!(
                "- `{}` | alias `{}` | handle `{}`",
                file.name, file.alias, file.handle
            ));
        }

        lines.push(String::new());
        lines.push(
            "This directory is a human-readable projection of the canonical artifact bundle."
                .to_string(),
        );
        lines.push(
            "Edit neither these files nor this README; rebuild the artifact instead.".to_string(),
        );

        lines.join("\n")
    }

    /// Inspects an artifact by ID — returns its manifest.
    pub fn inspect(&self, artifact_id: &str) -> anyhow::Result<ArtifactBundle> {
        anyhow::ensure!(
            artifact_id.starts_with(ARTIFACT_ID_PREFIX),
            "invalid artifact ID format: expected '{}...' prefix",
            ARTIFACT_ID_PREFIX
        );

        let manifest_path = self.artifacts_dir.join(artifact_id).join("manifest.json");
        anyhow::ensure!(
            manifest_path.exists(),
            "artifact '{}' not found",
            artifact_id
        );

        let json = std::fs::read_to_string(&manifest_path)?;
        let bundle: ArtifactBundle = serde_json::from_str(&json)?;
        anyhow::ensure!(
            bundle.artifact_id == artifact_id,
            "artifact '{}' manifest claims different artifact_id '{}'",
            artifact_id,
            bundle.artifact_id
        );
        let expected_digest = Self::compute_bundle_digest(&bundle)?;
        anyhow::ensure!(
            bundle.digest == expected_digest,
            "artifact '{}' digest mismatch: manifest has '{}' but recomputed digest is '{}'. Possible tampering or corruption.",
            artifact_id,
            bundle.digest,
            expected_digest
        );
        Ok(bundle)
    }

    /// Resolves artifact files for sandbox mounting.
    /// Returns (name, content_bytes) pairs.
    pub fn resolve_files(&self, artifact_id: &str) -> anyhow::Result<Vec<(String, Vec<u8>)>> {
        let bundle = self.inspect(artifact_id)?;
        let mut result = Vec::new();

        for file in &bundle.files {
            let content = self.content_store.read(&file.handle)?;
            result.push((file.name.clone(), content));
        }

        Ok(result)
    }

    /// Lists all artifact IDs.
    pub fn list(&self) -> anyhow::Result<Vec<String>> {
        let index = self.load_index()?;
        let mut ids: Vec<String> = index.entries.keys().cloned().collect();
        ids.sort();
        Ok(ids)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::content_store::ContentVisibility;
    use tempfile::tempdir;

    #[test]
    fn test_artifact_build_and_inspect() {
        let temp = tempdir().unwrap();
        let gw = temp.path().join(".gateway");
        std::fs::create_dir_all(&gw).unwrap();

        let store = ArtifactStore::new(&gw).unwrap();
        let content_store = ContentStore::new(&gw).unwrap();

        // Write some content
        let h1 = content_store.write(b"print('hello')").unwrap();
        content_store
            .register_name("session-1", "main.py", &h1)
            .unwrap();

        let h2 = content_store.write(b"def util(): pass").unwrap();
        content_store
            .register_name("session-1", "utils.py", &h2)
            .unwrap();

        // Build artifact
        let bundle = store
            .build(
                &["main.py".into(), "utils.py".into()],
                Some(&["main.py".into()]),
                "session-1",
            )
            .unwrap();

        assert!(bundle.artifact_id.starts_with("art_"));
        assert_eq!(bundle.files.len(), 2);
        assert_eq!(bundle.entrypoints, vec!["main.py"]);
        assert!(bundle.digest.starts_with("sha256:"));
        assert_eq!(bundle.builder_session_id, "session-1");

        // Inspect artifact
        let inspected = store.inspect(&bundle.artifact_id).unwrap();
        assert_eq!(inspected.artifact_id, bundle.artifact_id);
        assert_eq!(inspected.files.len(), 2);
        assert_eq!(inspected.digest, bundle.digest);

        // Resolve files
        let resolved = store.resolve_files(&bundle.artifact_id).unwrap();
        assert_eq!(resolved.len(), 2);
        assert_eq!(resolved[0].0, "main.py");
        assert_eq!(resolved[0].1, b"print('hello')");
    }

    #[test]
    fn test_artifact_build_materializes_session_projection() {
        let temp = tempdir().unwrap();
        let gw = temp.path().join(".gateway");
        std::fs::create_dir_all(&gw).unwrap();

        let store = ArtifactStore::new(&gw).unwrap();
        let content_store = ContentStore::new(&gw).unwrap();

        let h1 = content_store.write(b"print('hello world')").unwrap();
        content_store
            .register_name("demo-session/coder.default-abc", "weather_fetch.py", &h1)
            .unwrap();

        let h2 = content_store.write(b"assert True").unwrap();
        content_store
            .register_name(
                "demo-session/coder.default-abc",
                "tests/test_weather_fetch.py",
                &h2,
            )
            .unwrap();

        let bundle = store
            .build(
                &[
                    "weather_fetch.py".into(),
                    "tests/test_weather_fetch.py".into(),
                ],
                Some(&["weather_fetch.py".into()]),
                "demo-session/coder.default-abc",
            )
            .unwrap();

        let session_artifact_dir = gw
            .join("sessions")
            .join("demo-session")
            .join("artifacts")
            .join(&bundle.artifact_id);

        let projected_main =
            std::fs::read_to_string(session_artifact_dir.join("weather_fetch.py")).unwrap();
        assert_eq!(projected_main, "print('hello world')");

        let projected_test =
            std::fs::read_to_string(session_artifact_dir.join("tests/test_weather_fetch.py"))
                .unwrap();
        assert_eq!(projected_test, "assert True");

        let readme = std::fs::read_to_string(session_artifact_dir.join("README.md")).unwrap();
        assert!(readme.contains(&bundle.artifact_id));
        assert!(readme.contains("weather_fetch.py"));
        assert!(readme.contains("tests/test_weather_fetch.py"));
    }

    #[cfg(unix)]
    #[test]
    fn test_artifact_projection_uses_symlink_to_canonical_blob() {
        let temp = tempdir().unwrap();
        let gw = temp.path().join(".gateway");
        std::fs::create_dir_all(&gw).unwrap();

        let store = ArtifactStore::new(&gw).unwrap();
        let content_store = ContentStore::new(&gw).unwrap();

        let handle = content_store.write(b"print('linked')").unwrap();
        content_store
            .register_name("demo-session/coder.default-abc", "main.py", &handle)
            .unwrap();

        let bundle = store
            .build(
                &["main.py".into()],
                Some(&["main.py".into()]),
                "demo-session/coder.default-abc",
            )
            .unwrap();

        let projected = gw
            .join("sessions")
            .join("demo-session")
            .join("artifacts")
            .join(&bundle.artifact_id)
            .join("main.py");

        let metadata = std::fs::symlink_metadata(&projected).unwrap();
        assert!(metadata.file_type().is_symlink());

        let target = std::fs::read_link(&projected).unwrap();
        assert_eq!(target, content_store.blob_path(&handle));
    }

    #[test]
    fn test_artifact_list() {
        let temp = tempdir().unwrap();
        let gw = temp.path().join(".gateway");
        std::fs::create_dir_all(&gw).unwrap();

        let store = ArtifactStore::new(&gw).unwrap();
        let content_store = ContentStore::new(&gw).unwrap();

        let h = content_store.write(b"data").unwrap();
        content_store.register_name("s1", "data.txt", &h).unwrap();

        let b1 = store.build(&["data.txt".into()], None, "s1").unwrap();
        let b2 = store.build(&["data.txt".into()], None, "s1").unwrap();

        assert_eq!(b1.artifact_id, b2.artifact_id);
        assert!(!b1.reused);
        assert!(b2.reused);

        let ids = store.list().unwrap();
        assert_eq!(ids.len(), 1);
        assert!(ids.contains(&b1.artifact_id));
    }

    #[test]
    fn test_artifact_build_validates_entrypoints() {
        let temp = tempdir().unwrap();
        let gw = temp.path().join(".gateway");
        std::fs::create_dir_all(&gw).unwrap();

        let store = ArtifactStore::new(&gw).unwrap();
        let content_store = ContentStore::new(&gw).unwrap();

        let h = content_store.write(b"main").unwrap();
        content_store.register_name("s1", "main.py", &h).unwrap();

        // Entrypoint not in inputs should fail
        let result = store.build(&["main.py".into()], Some(&["missing.py".into()]), "s1");
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("entrypoint 'missing.py' not found"));
    }

    #[test]
    fn test_artifact_immutability() {
        let temp = tempdir().unwrap();
        let gw = temp.path().join(".gateway");
        std::fs::create_dir_all(&gw).unwrap();

        let store = ArtifactStore::new(&gw).unwrap();
        let content_store = ContentStore::new(&gw).unwrap();

        let h = content_store.write(b"v1").unwrap();
        content_store.register_name("s1", "file.txt", &h).unwrap();

        let bundle = store.build(&["file.txt".into()], None, "s1").unwrap();

        let bundle2 = store.build(&["file.txt".into()], None, "s1").unwrap();

        assert_eq!(bundle.artifact_id, bundle2.artifact_id);
        assert!(!bundle.reused);
        assert!(bundle2.reused);
        assert_eq!(bundle.files[0].handle, bundle2.files[0].handle);
    }

    #[test]
    fn test_artifact_reuse_rejects_manifest_identity_mismatch() {
        let temp = tempdir().unwrap();
        let gw = temp.path().join(".gateway");
        std::fs::create_dir_all(&gw).unwrap();

        let store = ArtifactStore::new(&gw).unwrap();
        let content_store = ContentStore::new(&gw).unwrap();

        let h = content_store.write(b"payload").unwrap();
        content_store.register_name("s1", "a.txt", &h).unwrap();

        let bundle = store.build(&["a.txt".into()], None, "s1").unwrap();
        let manifest_path = gw
            .join("artifacts")
            .join(&bundle.artifact_id)
            .join("manifest.json");
        let mut raw: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&manifest_path).unwrap()).unwrap();
        if let Some(files) = raw.get_mut("files").and_then(|v| v.as_array_mut()) {
            if let Some(first) = files.first_mut() {
                if let Some(obj) = first.as_object_mut() {
                    obj.insert(
                        "handle".to_string(),
                        serde_json::json!("sha256:0000000000000000000000000000000000000000000000000000000000000000"),
                    );
                }
            }
        }
        let mut perms = std::fs::metadata(&manifest_path).unwrap().permissions();
        perms.set_readonly(false);
        std::fs::set_permissions(&manifest_path, perms).unwrap();
        std::fs::write(&manifest_path, serde_json::to_string_pretty(&raw).unwrap()).unwrap();

        let err = store
            .build(&["a.txt".into()], None, "s1")
            .expect_err("corrupt manifest must block reuse");
        assert!(
            err.to_string().contains("identity mismatch")
                || err.to_string().contains("digest mismatch"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn test_artifact_inspect_rejects_digest_mismatch() {
        let temp = tempdir().unwrap();
        let gw = temp.path().join(".gateway");
        std::fs::create_dir_all(&gw).unwrap();

        let store = ArtifactStore::new(&gw).unwrap();
        let content_store = ContentStore::new(&gw).unwrap();

        let handle = content_store.write(b"print('hello')").unwrap();
        content_store
            .register_name("session-1", "main.py", &handle)
            .unwrap();

        let bundle = store.build(&["main.py".to_string()], Some(&["main.py".to_string()]), "session-1").unwrap();
        let manifest_path = gw
            .join("artifacts")
            .join(&bundle.artifact_id)
            .join("manifest.json");

        let mut raw: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&manifest_path).unwrap()).unwrap();
        raw["digest"] = serde_json::Value::String("sha256:deadbeef".to_string());
        let mut perms = std::fs::metadata(&manifest_path).unwrap().permissions();
        perms.set_readonly(false);
        std::fs::set_permissions(&manifest_path, perms).unwrap();
        std::fs::write(&manifest_path, serde_json::to_string_pretty(&raw).unwrap()).unwrap();

        let err = store.inspect(&bundle.artifact_id).expect_err("tampered digest must fail inspect");
        assert!(err.to_string().contains("digest mismatch"));
    }

    #[test]
    fn test_artifact_root_session_visibility() {
        let temp = tempdir().unwrap();
        let gw = temp.path().join(".gateway");
        std::fs::create_dir_all(&gw).unwrap();

        let store = ArtifactStore::new(&gw).unwrap();
        let content_store = ContentStore::new(&gw).unwrap();

        let root = "demo-session";
        let child = "demo-session/coder-abc";

        content_store.set_root_session(child, root).unwrap();

        // Child writes content with session visibility
        let h = content_store.write(b"child code").unwrap();
        content_store
            .register_name_with_visibility(child, "code.py", &h, ContentVisibility::Session)
            .unwrap();

        // Root can build artifact from child's session-visible content
        let bundle = store.build(&["code.py".into()], None, root).unwrap();
        assert_eq!(bundle.files.len(), 1);
    }
}
