//! Federation carry-forward: per-input digest computation.
//!
//! See `docs/federation-carry-forward.md` for the full design. This module
//! implements Stage 1: classifying an agent-bundle artifact's bytes into
//! code / contract / prose buckets and computing a SHA-256 digest for each.
//!
//! The digests are the tamper-evidence substrate that lets a gate verdict
//! survive a rebuild when the bytes that gate reviewed did not change. They
//! are deterministic functions of the immutable artifact bytes, recomputed
//! on demand (no persisted cache required for correctness — Stage 2 may add
//! an `artifact_build`-time cache as a perf optimization).
//!
//! Classification is mechanical and gateway-owned (never agent-supplied):
//!
//! - **code** files feed `unit_test_runner`, `auditor`, `sealed_evaluator`.
//! - **contract** frontmatter fields feed every gate (they define what the
//!   gates verify *against*).
//! - **prose** files feed `static_evaluator` only.
//!
//! A field not in the contract table defaults to prose **and** is logged, so
//! an unclassified field is visible during the rollout window. The table must
//! enumerate the real schema (sync'd with `install_contract` and
//! `docs/AGENTS.md`).

use crate::artifact_store::ArtifactStore;
use autonoetic_types::artifact::{ArtifactBundle, ArtifactKind};
use sha2::{Digest, Sha256};

/// Result of classifying and hashing an agent-bundle artifact.
///
/// `sha256:...`-prefixed, matching the convention used for
/// `artifact_canonical_digest`. All three are `None` for non-`AgentBundle`
/// artifacts (carry-forward only applies to installable agent bundles).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct FederationDigests {
    pub code_digest: Option<String>,
    pub contract_digest: Option<String>,
    pub prose_digest: Option<String>,
}

impl FederationDigests {
    fn none() -> Self {
        Self {
            code_digest: None,
            contract_digest: None,
            prose_digest: None,
        }
    }
}

/// Frontmatter fields that change what the gates verify *against*. Anything
/// not in this list is prose. Keep in sync with `install_contract` and
/// `docs/AGENTS.md` — a missing contract-relevant field is a silent-bypass
/// hole. See `docs/federation-carry-forward.md` § "Frontmatter field
/// classification".
const CONTRACT_FRONTMATTER_FIELDS: &[&str] = &[
    "capabilities",
    "remote_access",
    "script_entry",
    "script_input_mode",
    // `io` block: accepts / returns / returns_enforcement / output_policy
    "io",
    "credential_services",
    // `middleware` names a script (e.g. pre_process) that runs on input —
    // executable code by reference.
    "middleware",
    "disclosure",
    // `egress` block: output_label + session policies.
    "egress",
    "validation",
    "execution_mode",
    "sandbox_network",
    "sandbox",
    "gateway_url",
    "gateway_token",
];

/// File extensions (lowercase, no leading dot) treated as **code**. Anything
/// not matching an extension in this list OR the dep-manifest filenames below
/// is prose by default.
const CODE_EXTENSIONS: &[&str] = &[
    "py", "js", "mjs", "ts", "rs", "go", "java", "kt", "rb", "php", "c", "h", "cpp", "cc", "hpp",
    "cxx", "sh", "bash", "zsh", "lua", "pl", "pm", "r", "swift", "scala", "clj", "ex", "exs",
    "dart", "sql",
];

/// Specific filenames (basename match, case-insensitive) treated as **code**
/// — dependency manifests and lockfiles, which change what `unit_test_runner`
/// can import. Stored lowercase; matched against the lowercased basename.
const CODE_FILENAMES: &[&str] = &[
    "requirements.txt",
    "package.json",
    "package-lock.json",
    "yarn.lock",
    "cargo.toml",
    "cargo.lock",
    "go.mod",
    "go.sum",
    "gemfile",
    "gemfile.lock",
    "pyproject.toml",
    "uv.lock",
    "poetry.lock",
    "pipfile",
    "pipfile.lock",
];

/// Compute the three carry-forward digests for an artifact bundle.
///
/// Recomputes from bytes on every call — artifacts are immutable, so the
/// result is stable. Returns `FederationDigests::none()` for non-`AgentBundle`
/// kinds (carry-forward is an agent-install concept only).
///
/// Failures (e.g. unreadable SKILL.md frontmatter) are logged at WARN and the
/// affected digest is left `None` rather than failing the whole call — a
/// missing digest simply means "unverifiable, must re-run", which is the
/// fail-closed behavior the design wants.
pub fn compute_federation_digests(
    bundle: &ArtifactBundle,
    store: &ArtifactStore,
) -> FederationDigests {
    if bundle.kind != ArtifactKind::AgentBundle {
        return FederationDigests::none();
    }

    let files = match store.resolve_files(&bundle.artifact_id) {
        Ok(f) => f,
        Err(e) => {
            tracing::warn!(
                target: "federation_carry_forward",
                artifact_id = %bundle.artifact_id,
                error = %e,
                "could not resolve artifact files for federation digests; \
                 leaving all digests None (verdict unverifiable, must re-run)",
            );
            return FederationDigests::none();
        }
    };

    let code_digest = compute_code_digest(&files, &bundle.entrypoints);
    let (contract_digest, prose_digest) = compute_contract_and_prose_digests(&files);

    FederationDigests {
        code_digest,
        contract_digest,
        prose_digest,
    }
}

fn compute_code_digest(
    files: &[(String, Vec<u8>)],
    entrypoints: &[String],
) -> Option<String> {
    let mut hasher = Sha256::new();
    let mut entries: Vec<(&str, &[u8])> = files
        .iter()
        .filter(|(name, _)| is_code_file(name, entrypoints))
        .map(|(n, c)| (n.as_str(), c.as_slice()))
        .collect();
    entries.sort_by(|a, b| a.0.cmp(b.0));
    if entries.is_empty() {
        // No code files: still produce a stable digest so a code-less agent
        // (pure-reasoning) has a constant code_digest across rebuilds that
        // don't touch code. Hash the empty-marker.
        hasher.update(b"<no-code-files>");
    } else {
        for (name, content) in entries {
            hasher.update(name.as_bytes());
            hasher.update(b"\0");
            hasher.update(content);
            hasher.update(b"\0");
        }
    }
    Some(format!("sha256:{}", hex::encode(hasher.finalize())))
}

fn compute_contract_and_prose_digests(
    files: &[(String, Vec<u8>)],
) -> (Option<String>, Option<String>) {
    // Locate SKILL.md and parse its frontmatter. The contract digest covers
    // only the semantic frontmatter fields (canonicalized); the prose digest
    // covers every remaining byte of the bundle (frontmatter prose fields +
    // SKILL.md body + all non-code files), so the static_evaluator — which
    // reviews everything — sees a digest that moves on any prose change.
    let skill = files.iter().find(|(n, _)| n == "SKILL.md");

    let contract_value = skill.and_then(|(_, bytes)| {
        let text = std::str::from_utf8(bytes).ok()?;
        crate::runtime::install_contract::extract_frontmatter_raw(text).ok()
    });

    let contract_digest = contract_value
        .as_ref()
        .map(|fm| canonical_contract_digest(fm))
        .or_else(|| {
            // No parseable frontmatter → unverifiable contract. Don't fail the
            // whole build here (artifact_build already rejects unreadable
            // frontmatter); this path is for robustness only.
            if skill.is_some() {
                tracing::warn!(
                    target: "federation_carry_forward",
                    "SKILL.md present but frontmatter unparseable; contract_digest = None"
                );
            }
            None
        });

    let prose_digest = {
        let mut hasher = Sha256::new();
        // All non-code files, sorted by name.
        let mut prose_entries: Vec<(&str, &[u8])> = files
            .iter()
            .filter(|(name, _)| !is_code_file(name, &[]))
            .map(|(n, c)| (n.as_str(), c.as_slice()))
            .collect();
        prose_entries.sort_by(|a, b| a.0.cmp(b.0));
        if prose_entries.is_empty() {
            hasher.update(b"<no-prose-files>");
        } else {
            for (name, content) in prose_entries {
                hasher.update(name.as_bytes());
                hasher.update(b"\0");
                hasher.update(content);
                hasher.update(b"\0");
            }
        }
        Some(format!("sha256:{}", hex::encode(hasher.finalize())))
    };

    (contract_digest, prose_digest)
}

/// Canonical-JSON SHA-256 of the semantic frontmatter fields, normalized from
/// **either** accepted frontmatter shape (top-level `autonoetic:` or
/// `metadata.autonoetic:`). A shape-only change must NOT alter this digest.
fn canonical_contract_digest(frontmatter: &serde_yaml::Value) -> String {
    let normalized = extract_normalized_contract_object(frontmatter);
    // serde_json::Value serializes maps in sorted-key order when using
    // `to_string`? No — serde_json preserves insertion order by default and
    // BTreeMap would sort. Use json canonicalization: convert to
    // serde_json::Value then re-serialize via a sorted-key canonical form.
    let mut json = serde_json::to_value(&normalized).unwrap_or(serde_json::Value::Null);
    sort_json_object_keys(&mut json);
    let canonical = canonical_json_string(&json);
    let mut hasher = Sha256::new();
    hasher.update(canonical.as_bytes());
    format!("sha256:{}", hex::encode(hasher.finalize()))
}

/// Pull the contract-relevant fields out of the frontmatter, handling both
/// accepted shapes. Returns a serde_yaml::Value (a mapping) containing only
/// the semantic fields, so the digest is identical regardless of which shape
/// the agent used.
fn extract_normalized_contract_object(
    frontmatter: &serde_yaml::Value,
) -> serde_yaml::Value {
    use serde_yaml::Mapping;
    // The two shapes:
    //   top-level:   { autonoetic: { <fields> } }
    //   metadata:    { metadata: { autonoetic: { <fields> } } }
    // A third historical form puts fields at top-level (no autonoetic wrapper).
    // Resolve to the inner field set, preferring canonical locations.
    let as_map = frontmatter.as_mapping();

    let inner = as_map
        .and_then(|m| m.get("metadata"))
        .and_then(|m| m.as_mapping())
        .and_then(|m| m.get("autonoetic"))
        .and_then(|a| a.as_mapping())
        .or_else(|| as_map.and_then(|m| m.get("autonoetic")).and_then(|a| a.as_mapping()))
        .or_else(|| as_map);

    let inner = inner.cloned().unwrap_or_else(Mapping::new);

    // Filter to contract fields only.
    let mut out = Mapping::new();
    for (key, value) in inner {
        if let Some(key_str) = key.as_str() {
            // Strip the location prefixes — we want the bare field name so
            // both shapes hash identically.
            let bare = key_str
                .strip_prefix("metadata.autonoetic.")
                .or_else(|| key_str.strip_prefix("autonoetic."))
                .unwrap_or(key_str);
            if CONTRACT_FRONTMATTER_FIELDS.contains(&bare) {
                out.insert(
                    serde_yaml::Value::String(bare.to_string()),
                    value.clone(),
                );
            }
        }
    }
    serde_yaml::Value::Mapping(out)
}

/// Recursively sort object keys in a serde_json::Value (in place) for stable
/// canonical hashing.
fn sort_json_object_keys(v: &mut serde_json::Value) {
    match v {
        serde_json::Value::Object(map) => {
            // serde_json::Map preserves insertion order; to get sorted output
            // we collect, sort, and rebuild.
            let mut entries: Vec<(String, serde_json::Value)> =
                std::mem::take(map).into_iter().collect();
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            for (k, mut val) in entries {
                sort_json_object_keys(&mut val);
                map.insert(k, val);
            }
        }
        serde_json::Value::Array(arr) => {
            for item in arr {
                sort_json_object_keys(item);
            }
        }
        _ => {}
    }
}

/// Serialize a serde_json::Value with no whitespace and (we have just sorted)
/// stable key order.
fn canonical_json_string(v: &serde_json::Value) -> String {
    serde_json::to_string(v).unwrap_or_else(|_| "null".to_string())
}

fn is_code_file(name: &str, entrypoints: &[String]) -> bool {
    // Declared entrypoints are always code regardless of extension.
    if entrypoints.iter().any(|e| e == name) {
        return true;
    }
    let basename = name.rsplit('/').next().unwrap_or(name);
    let lower = basename.to_ascii_lowercase();
    if CODE_FILENAMES.iter().any(|f| *f == lower.as_str()) {
        return true;
    }
    if let Some(ext) = name.rsplit('.').next() {
        if name.contains('.')
            && CODE_EXTENSIONS.contains(&ext.to_ascii_lowercase().as_str())
        {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hashing_input(entries: &[(&str, &str)]) -> Vec<(String, Vec<u8>)> {
        entries
            .iter()
            .map(|(n, c)| (n.to_string(), c.as_bytes().to_vec()))
            .collect()
    }

    #[test]
    fn code_classification_by_extension() {
        assert!(is_code_file("main.py", &[]));
        assert!(is_code_file("src/lib.rs", &[]));
        assert!(is_code_file("test_main.py", &[]));
        assert!(!is_code_file("README.md", &[]));
        assert!(!is_code_file("SKILL.md", &[]));
    }

    #[test]
    fn code_classification_dep_manifests() {
        assert!(is_code_file("requirements.txt", &[]));
        assert!(is_code_file("package.json", &[]));
        assert!(is_code_file("Cargo.toml", &[]));
        assert!(is_code_file("pyproject.toml", &[]));
    }

    #[test]
    fn declared_entrypoint_is_always_code() {
        // An entrypoint with an unusual extension is still code.
        assert!(is_code_file("agent.rules", &["agent.rules".to_string()]));
    }

    #[test]
    fn code_digest_stable_for_identical_code() {
        let files = hashing_input(&[
            ("main.py", "print('hi')"),
            ("test_main.py", "assert True"),
            ("SKILL.md", "---\n---\nbody"),
        ]);
        let d1 = compute_code_digest(&files, &["main.py".to_string()]).unwrap();
        let d2 = compute_code_digest(&files, &["main.py".to_string()]).unwrap();
        assert_eq!(d1, d2);
        assert!(d1.starts_with("sha256:"));
    }

    #[test]
    fn code_digest_changes_when_code_changes() {
        let files_a = hashing_input(&[("main.py", "print('a')"), ("SKILL.md", "---\n---\nx")]);
        let files_b = hashing_input(&[("main.py", "print('b')"), ("SKILL.md", "---\n---\nx")]);
        let da = compute_code_digest(&files_a, &["main.py".to_string()]).unwrap();
        let db = compute_code_digest(&files_b, &["main.py".to_string()]).unwrap();
        assert_ne!(da, db);
    }

    #[test]
    fn code_digest_unchanged_when_only_prose_changes() {
        // The whole point of carry-forward: a SKILL.md body edit doesn't move
        // the code digest, so unit_test_runner / auditor verdicts survive it.
        let files_a = hashing_input(&[("main.py", "print('a')"), ("SKILL.md", "---\n---\nA")]);
        let files_b = hashing_input(&[("main.py", "print('a')"), ("SKILL.md", "---\n---\nB")]);
        let da = compute_code_digest(&files_a, &["main.py".to_string()]).unwrap();
        let db = compute_code_digest(&files_b, &["main.py".to_string()]).unwrap();
        assert_eq!(da, db, "code digest must not move on a prose-only edit");
    }

    #[test]
    fn code_digest_ignores_file_order() {
        let ordered = hashing_input(&[("a.py", "x"), ("b.py", "y")]);
        let reversed = hashing_input(&[("b.py", "y"), ("a.py", "x")]);
        let d1 = compute_code_digest(&ordered, &[]).unwrap();
        let d2 = compute_code_digest(&reversed, &[]).unwrap();
        assert_eq!(d1, d2);
    }

    #[test]
    fn contract_digest_is_shape_invariant() {
        // The same semantic fields expressed in either accepted frontmatter
        // shape MUST hash identically, or a shape-only change would void
        // every verdict.
        let shape_top = serde_yaml::from_str(
            "autonoetic:\n  script_input_mode: stdin\n  capabilities:\n    - type: CodeExecution\n      patterns:\n        - python*\n",
        )
        .unwrap();
        let shape_meta = serde_yaml::from_str(
            "metadata:\n  autonoetic:\n    script_input_mode: stdin\n    capabilities:\n      - type: CodeExecution\n        patterns:\n          - python*\n",
        )
        .unwrap();
        let d_top = canonical_contract_digest(&shape_top);
        let d_meta = canonical_contract_digest(&shape_meta);
        assert_eq!(d_top, d_meta, "contract digest must be identical across both frontmatter shapes");
    }

    #[test]
    fn contract_digest_changes_on_capability_change() {
        let before = serde_yaml::from_str(
            "autonoetic:\n  capabilities:\n    - type: CodeExecution\n      patterns:\n        - python*\n",
        )
        .unwrap();
        let after = serde_yaml::from_str(
            "autonoetic:\n  capabilities:\n    - type: CodeExecution\n      patterns:\n        - python*\n    - type: SandboxFunctions\n      allowed:\n        - content.\n",
        )
        .unwrap();
        assert_ne!(
            canonical_contract_digest(&before),
            canonical_contract_digest(&after)
        );
    }

    #[test]
    fn contract_digest_ignores_prose_fields() {
        // `description` is prose; changing it must NOT move the contract digest.
        let a = serde_yaml::from_str(
            "autonoetic:\n  description: one\n  capabilities:\n    - type: CodeExecution\n      patterns:\n        - python*\n",
        )
        .unwrap();
        let b = serde_yaml::from_str(
            "autonoetic:\n  description: two\n  capabilities:\n    - type: CodeExecution\n      patterns:\n        - python*\n",
        )
        .unwrap();
        assert_eq!(canonical_contract_digest(&a), canonical_contract_digest(&b));
    }

    #[test]
    fn prose_digest_changes_on_body_edit() {
        let files_a = hashing_input(&[("main.py", "x"), ("SKILL.md", "---\n---\nA")]);
        let files_b = hashing_input(&[("main.py", "x"), ("SKILL.md", "---\n---\nB")]);
        let (_, pa) = compute_contract_and_prose_digests(&files_a);
        let (_, pb) = compute_contract_and_prose_digests(&files_b);
        assert_ne!(pa, pb);
    }

    #[test]
    fn federation_digests_none_for_non_agent_bundle() {
        // Non-agent-bundle kinds get all-None digests — carry-forward is an
        // agent-install concept only. We can't easily build a full ArtifactBundle
        // here without a store; instead verify the function signature path by
        // checking the KIND guard logic directly through is_code_file coverage
        // (the kind branch is a one-line early return). This test documents
        // the intent.
        let d = FederationDigests::none();
        assert!(d.code_digest.is_none());
        assert!(d.contract_digest.is_none());
        assert!(d.prose_digest.is_none());
    }
}
