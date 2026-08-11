//! Federation carry-forward: per-input digest computation.
//!
//! See `docs/federation-carry-forward.md` for the full design (the design spec
//! lands with #1067; until that merges the path resolves only on that branch).
//! This module implements Stage 1: classifying an agent-bundle artifact's bytes
//! into code / contract / prose buckets and computing a SHA-256 digest for each.
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
//! A field in neither the contract table nor `KNOWN_PROSE_FRONTMATTER_FIELDS`
//! defaults to prose **and** is logged at DEBUG, so an unclassified field is
//! visible during the rollout window. (The design spec says INFO; these digests
//! are recomputed on every `artifact_inspect` / `artifact_diff` — a read path —
//! so INFO would be steady-state noise. DEBUG keeps the tripwire without it.)
//! The table must enumerate the real schema (sync'd with `install_contract` and
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
/// classification" (design spec; lands with #1067).
///
/// An entry containing `.` is a **nested path** resolved inside the autonoetic
/// block (e.g. `runtime.sandbox`). Nested paths exist so a contract-relevant
/// field can be covered without pulling in its whole parent block: `runtime`
/// also holds the gateway-autofilled `gateway_version` / `sdk_version`, and
/// hashing those would void every carry on an unrelated gateway upgrade.
const CONTRACT_FRONTMATTER_FIELDS: &[&str] = &[
    "capabilities",
    "remote_access",
    "script_entry",
    "script_input_mode",
    // `io` block: accepts / returns / returns_enforcement / output_policy
    "io",
    // Real bundles also carry `output_policy` as a sibling of `io`.
    "output_policy",
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
    // `open_web` gates wildcard NetworkAccess — it is precisely the network
    // posture the auditor and static_evaluator review against, so it must void
    // their verdicts when it flips.
    "open_web",
    // Tool surface is privilege surface: `auditor.default` reviews the agent
    // "given allowed_tool_tiers (or the default tier)", so a change to either
    // list invalidates that review.
    "allowed_tool_tiers",
    "excluded_tools",
    // Pulls external skill instructions/code in by reference.
    "agentskills_import",
    // Legacy flat form of the sandbox backend. Both accepted shapes nest it
    // under `runtime:`, covered by the nested path below; kept so a
    // hand-authored flat manifest is not silently unclassified.
    "sandbox",
    "gateway_url",
    "gateway_token",
    // Nested: the sandbox backend and the pinned runtime closure change the
    // execution semantics every gate assumes.
    "runtime.sandbox",
    "runtime.runtime_lock",
    "runtime.type",
];

/// Frontmatter fields deliberately classified as **prose** — presentation,
/// scheduling, and model-selection metadata that the gates do not verify
/// against. Listed explicitly (rather than falling through) so the
/// unclassified-field tripwire below only fires on fields nobody has ruled on
/// yet. See the classification table in the design spec.
const KNOWN_PROSE_FRONTMATTER_FIELDS: &[&str] = &[
    "version",
    // agent.{name,description} are presentation; agent.id is identity, which
    // is bound by the revision/slot, not by the reviewed bytes.
    "agent",
    "name",
    "description",
    // The reasoning model is not something a gate reviews the code against.
    "llm_preset",
    "llm_overrides",
    "llm_config",
    "limits",
    "background",
    "compression",
    "loop_guard",
    "singleton",
    "resident_idle_ttl_secs",
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
    // The pinned runtime closure: `unit_test_runner` imports through its
    // layers, so a dependency/layer change alters test execution and must void
    // the code gates rather than reading as a prose edit.
    "runtime.lock",
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
    let (contract_digest, prose_digest) =
        compute_contract_and_prose_digests(&files, &bundle.entrypoints);

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
    entrypoints: &[String],
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
        // All non-code files, sorted by name. `entrypoints` must be passed
        // here too: code and prose are complements, and a declared entrypoint
        // with a non-standard extension is code by declaration. Omitting it
        // would land that file in *both* digests, so a code-only edit would
        // also move `prose_digest` and needlessly void static_evaluator —
        // exactly the re-run this feature exists to avoid.
        let mut prose_entries: Vec<(&str, &[u8])> = files
            .iter()
            .filter(|(name, _)| !is_code_file(name, entrypoints))
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
    for (key, value) in &inner {
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
            } else if !is_classified_frontmatter_field(bare) {
                // The rollout tripwire: a field nobody has ruled on defaults to
                // prose, which is fail-safe for the *contract* digest but means
                // a genuinely contract-relevant field would carry silently.
                // Surface it so the table can be corrected.
                tracing::debug!(
                    target: "federation_carry_forward",
                    field = %bare,
                    "frontmatter field is in neither the contract nor the known-prose \
                     table; defaulting to prose. If it changes what a gate verifies \
                     against, add it to CONTRACT_FRONTMATTER_FIELDS.",
                );
            }
        }
    }

    // Nested contract paths (e.g. `runtime.sandbox`), keyed by the dotted path
    // so they cannot collide with a flat field of the same leaf name.
    for path in CONTRACT_FRONTMATTER_FIELDS
        .iter()
        .filter(|f| f.contains('.'))
    {
        if let Some(value) = lookup_nested_field(&inner, path) {
            out.insert(
                serde_yaml::Value::String((*path).to_string()),
                value.clone(),
            );
        }
    }

    serde_yaml::Value::Mapping(out)
}

/// Is this frontmatter field accounted for by either classification table?
///
/// A field is also "classified" when it is the parent of a nested contract path
/// (`runtime` for `runtime.sandbox`) — the parent itself is not hashed, but the
/// decision about it has been made.
fn is_classified_frontmatter_field(bare: &str) -> bool {
    KNOWN_PROSE_FRONTMATTER_FIELDS.contains(&bare)
        || CONTRACT_FRONTMATTER_FIELDS
            .iter()
            .any(|f| f.split_once('.').is_some_and(|(parent, _)| parent == bare))
}

/// Resolve a dotted path (`runtime.sandbox`) inside a frontmatter mapping.
fn lookup_nested_field<'a>(
    map: &'a serde_yaml::Mapping,
    dotted: &str,
) -> Option<&'a serde_yaml::Value> {
    let mut segments = dotted.split('.');
    let mut current = map.get(segments.next()?)?;
    for segment in segments {
        current = current.as_mapping()?.get(segment)?;
    }
    Some(current)
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

// ---------------------------------------------------------------------------
// Stage 2: structured change-diff between two artifacts.
// ---------------------------------------------------------------------------

/// How a single file changed between two artifact revisions.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FileChangeKind {
    Added,
    Removed,
    Modified,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct FileChange {
    pub file: String,
    pub change: FileChangeKind,
}

/// Per-class digest-equality summary across two artifacts, plus the file-level
/// diff and a purely-mechanical carry-eligibility hint.
///
/// The carry-eligibility list is advisory: it reports which roles' reviewed
/// inputs are byte-identical, ignoring the strictness floor (Stage 3). A role
/// appears here iff `compute_artifact_diff` could find no digest difference
/// in the inputs that role reviews. The gateway still re-verifies at escalate
/// time and applies the configured strictness floor.
//
// This is a tool-output type only — never deserialized from agent input — so
// it derives Serialize but not Deserialize (the `Vec<&'static str>` role list
// would otherwise force an unwanted lifetime bound).
#[derive(Debug, Clone, serde::Serialize)]
pub struct ArtifactDiff {
    pub from_artifact_id: String,
    pub to_artifact_id: String,
    pub code_changed: bool,
    pub contract_changed: bool,
    pub prose_changed: bool,
    pub changed_files: Vec<FileChange>,
    /// Roles whose reviewed inputs are byte-identical between the two
    /// artifacts. Advisory only — the strictness floor (Stage 3) and the
    /// escalate-time verify are the real gate. Empty if anything changed that
    /// a code-reviewing gate cares about.
    pub carry_eligible_roles: Vec<&'static str>,
}

/// Compute the structured change-diff between two artifacts.
///
/// `from` is the prior artifact (the one whose verdicts might be carried);
/// `to` is the current rebuild. Both must be resolvable via the store.
pub fn compute_artifact_diff(
    from: &ArtifactBundle,
    to: &ArtifactBundle,
    store: &ArtifactStore,
) -> anyhow::Result<ArtifactDiff> {
    let from_digests = compute_federation_digests(from, store);
    let to_digests = compute_federation_digests(to, store);

    let code_changed = !digest_eq(&from_digests.code_digest, &to_digests.code_digest);
    let contract_changed =
        !digest_eq(&from_digests.contract_digest, &to_digests.contract_digest);
    let prose_changed = !digest_eq(&from_digests.prose_digest, &to_digests.prose_digest);

    let changed_files = compute_file_changes(from, to, store)?;

    // Code-reviewing gates (unit_test_runner, auditor, sealed_evaluator) all
    // review code_digest + contract_digest. static_evaluator reviews all
    // three, so it is only eligible when nothing changed — i.e. the same
    // artifact, which is not a real rebuild. We still include it for
    // completeness; Stage 3's verify path will reject it in practice when
    // strictness > off.
    let code_review_inputs_unchanged = !code_changed && !contract_changed;
    let mut carry_eligible_roles: Vec<&'static str> = Vec::new();
    if code_review_inputs_unchanged {
        carry_eligible_roles.push("unit_test_runner");
        carry_eligible_roles.push("auditor");
        carry_eligible_roles.push("sealed_evaluator");
    }
    if code_review_inputs_unchanged && !prose_changed {
        carry_eligible_roles.push("static_evaluator");
    }

    Ok(ArtifactDiff {
        from_artifact_id: from.artifact_id.clone(),
        to_artifact_id: to.artifact_id.clone(),
        code_changed,
        contract_changed,
        prose_changed,
        changed_files,
        carry_eligible_roles,
    })
}

fn digest_eq(a: &Option<String>, b: &Option<String>) -> bool {
    match (a, b) {
        (Some(x), Some(y)) => x == y,
        // If either digest is None (unverifiable — pre-feature record, or a
        // non-agent-bundle), treat them as NOT equal so the diff reports
        // "changed" and the planner re-runs the gate rather than assuming
        // carry is safe. Fail-closed.
        _ => false,
    }
}

fn compute_file_changes(
    from: &ArtifactBundle,
    to: &ArtifactBundle,
    store: &ArtifactStore,
) -> anyhow::Result<Vec<FileChange>> {
    // An unreadable artifact must NOT degrade to an empty file list: a caller
    // reading `changed_files: []` would take it as mechanical evidence that
    // nothing moved, which is exactly the unsafe carry assumption this module
    // exists to prevent. Missing evidence is an error, not a "no changes"
    // answer. (The digest side of the diff already fails closed — an
    // unreadable artifact yields `None` digests, and `digest_eq` reports
    // those as changed.)
    let from_files = store.resolve_files(&from.artifact_id).map_err(|e| {
        anyhow::anyhow!(
            "cannot read files for prior artifact '{}': {e}. Refusing to report an empty \
             file diff for an unreadable artifact.",
            from.artifact_id
        )
    })?;
    let to_files = store.resolve_files(&to.artifact_id).map_err(|e| {
        anyhow::anyhow!(
            "cannot read files for rebuilt artifact '{}': {e}. Refusing to report an empty \
             file diff for an unreadable artifact.",
            to.artifact_id
        )
    })?;

    use std::collections::BTreeMap;
    let from_map: BTreeMap<&str, &[u8]> = from_files
        .iter()
        .map(|(n, c)| (n.as_str(), c.as_slice()))
        .collect();
    let to_map: BTreeMap<&str, &[u8]> = to_files
        .iter()
        .map(|(n, c)| (n.as_str(), c.as_slice()))
        .collect();

    let mut changes = Vec::new();
    let mut all_names: std::collections::BTreeSet<&str> = from_map.keys().copied().collect();
    all_names.extend(to_map.keys().copied());
    for name in all_names {
        match (from_map.get(name), to_map.get(name)) {
            (None, Some(_)) => changes.push(FileChange {
                file: name.to_string(),
                change: FileChangeKind::Added,
            }),
            (Some(_), None) => changes.push(FileChange {
                file: name.to_string(),
                change: FileChangeKind::Removed,
            }),
            (Some(a), Some(b)) if a != b => changes.push(FileChange {
                file: name.to_string(),
                change: FileChangeKind::Modified,
            }),
            _ => {}
        }
    }
    Ok(changes)
}

// ---------------------------------------------------------------------------
// Stage 3: carry-eligibility verification.
// ---------------------------------------------------------------------------

use autonoetic_types::config::CarryForwardStrictness;
use autonoetic_types::promotion::{PromotionRecord, PromotionRole};

/// Why a carry-forward claim was rejected. Mapped to a structured
/// `carry_forward_rejected` error by `federation_escalate`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CarryRejection {
    /// No promotion record exists for the prior artifact, or it has no
    /// verdict recorded in the claimed role.
    PriorRecordMissing,
    /// The prior artifact has a record but the claimed role did not pass
    /// (or never ran) on it.
    PriorVerdictNotPassed,
    /// A reviewed digest differs between the prior record and the current
    /// artifact — the gate did not actually review the current bytes.
    DigestMismatch { field: &'static str },
    /// The claimed role is not eligible for carry at all (only code-reviewing
    /// gates carry; `static_evaluator` reviews prose and never carries).
    RoleNotCarryEligible,
    /// The configured strictness floor disallows this carry.
    StrictnessDisallowed,
}

impl CarryRejection {
    pub fn reason_code(&self) -> &'static str {
        match self {
            CarryRejection::PriorRecordMissing => "prior_record_missing",
            CarryRejection::PriorVerdictNotPassed => "prior_verdict_not_passed",
            CarryRejection::DigestMismatch { .. } => "digest_mismatch",
            CarryRejection::RoleNotCarryEligible => "role_not_carry_eligible",
            CarryRejection::StrictnessDisallowed => "strictness_disallowed",
        }
    }

    pub fn message(&self, role: &PromotionRole) -> String {
        match self {
            CarryRejection::PriorRecordMissing => format!(
                "no promotion record / verdict exists for the prior artifact in role '{}'",
                role.as_str()
            ),
            CarryRejection::PriorVerdictNotPassed => format!(
                "the prior artifact's role '{}' did not record a terminal pass",
                role.as_str()
            ),
            CarryRejection::DigestMismatch { field } => format!(
                "{field} differs between the prior artifact's record and the current artifact — \
                 role '{}' did not review the current bytes",
                role.as_str()
            ),
            CarryRejection::RoleNotCarryEligible => format!(
                "role '{}' is not eligible for carry-forward (only code-reviewing gates carry; \
                 static_evaluator reviews prose and always re-runs)",
                role.as_str()
            ),
            CarryRejection::StrictnessDisallowed => {
                "the configured carry_forward_strictness floor disallows this carry".to_string()
            }
        }
    }
}

/// Which digests a role reviews. Only code-reviewing gates may carry;
/// `static_evaluator` reviews all three digests so it never carries.
fn role_is_carry_eligible(role: &PromotionRole) -> bool {
    matches!(
        role,
        PromotionRole::UnitTestRunner | PromotionRole::Auditor | PromotionRole::SealedEvaluator
    )
}

/// Verify a single carry-forward claim.
///
/// The gateway calls this for each role-verdict in a `federation_escalate`
/// that carries `carried_from`. Returns `Ok(())` if the carry is sound;
/// `Err(CarryRejection)` otherwise (mapped to a structured rejection so the
/// planner re-runs just that gate).
///
/// Checks, in order:
/// 1. The role is carry-eligible (code-reviewing gates only).
/// 2. The prior artifact's record exists and the role recorded a pass on it.
/// 3. The digests the role reviews match between the prior record and the
///    current artifact (code + contract for code gates).
/// 4. The strictness floor allows the carry.
///
/// **Lineage:** we anchor lineage to the planner explicitly naming the prior
/// artifact ref within the same workflow, plus the content-addressed digest
/// match. Cross-lineage collisions would require identical per-class digests,
/// which content-addressing makes negligible. A dedicated
/// `source_artifact_ref` ancestry table is a tracked follow-up.
pub fn verify_carry_claim(
    role: &PromotionRole,
    prior_record: Option<&PromotionRecord>,
    current_digests: &FederationDigests,
    strictness: CarryForwardStrictness,
) -> Result<(), CarryRejection> {
    if !role_is_carry_eligible(role) {
        return Err(CarryRejection::RoleNotCarryEligible);
    }

    let prior = prior_record.ok_or(CarryRejection::PriorRecordMissing)?;

    // The claimed role must have recorded a terminal pass on the prior
    // artifact. `get_role_result` returns (pass, findings).
    let (prior_passed, _) = prior
        .get_role_result(role.as_str())
        .filter(|_| prior.has_role_verdict(role.as_str()))
        .ok_or(CarryRejection::PriorRecordMissing)?;
    if !prior_passed {
        return Err(CarryRejection::PriorVerdictNotPassed);
    }

    // The prior record must carry digests (recorded under this feature).
    // None digests = unverifiable = cannot carry.
    let prior_code = prior.code_digest.as_ref();
    let prior_contract = prior.contract_digest.as_ref();
    if prior_code.is_none() || prior_contract.is_none() {
        return Err(CarryRejection::PriorRecordMissing);
    }

    let cur_code = current_digests.code_digest.as_ref();
    let cur_contract = current_digests.contract_digest.as_ref();

    let code_changed = match (prior_code, cur_code) {
        (Some(a), Some(b)) => a != b,
        _ => true, // unverifiable → treat as changed (fail closed)
    };
    let contract_changed = match (prior_contract, cur_contract) {
        (Some(a), Some(b)) => a != b,
        _ => true,
    };

    if code_changed {
        return Err(CarryRejection::DigestMismatch { field: "code_digest" });
    }
    if contract_changed {
        return Err(CarryRejection::DigestMismatch {
            field: "contract_digest",
        });
    }

    if !strictness.allows_carry(code_changed, contract_changed, false) {
        return Err(CarryRejection::StrictnessDisallowed);
    }

    Ok(())
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
        let (_, pa) = compute_contract_and_prose_digests(&files_a, &["main.py".to_string()]);
        let (_, pb) = compute_contract_and_prose_digests(&files_b, &["main.py".to_string()]);
        assert_ne!(pa, pb);
    }

    /// Build a real artifact of `kind` in a temp store, so the kind guard is
    /// exercised through `compute_federation_digests` rather than asserted
    /// about a hand-made `FederationDigests`.
    fn build_artifact_of_kind(
        gateway_dir: &std::path::Path,
        kind: ArtifactKind,
    ) -> (ArtifactStore, ArtifactBundle) {
        let content_store = crate::runtime::content_store::ContentStore::new(gateway_dir).unwrap();
        for (name, body) in [
            ("main.py", "print('hi')\n"),
            ("SKILL.md", "---\nautonoetic:\n  script_input_mode: stdin\n---\nbody\n"),
        ] {
            let handle = content_store.write(body.as_bytes()).unwrap();
            content_store
                .register_name("session-1/coder.default-x", name, &handle)
                .unwrap();
        }
        let store = ArtifactStore::new(gateway_dir).unwrap();
        let bundle = store
            .build_with_kind(
                &["main.py".to_string(), "SKILL.md".to_string()],
                Some(&["main.py".to_string()]),
                None,
                kind,
                "session-1/coder.default-x",
            )
            .unwrap();
        (store, bundle)
    }

    #[test]
    fn federation_digests_none_for_non_agent_bundle() {
        // Carry-forward is an agent-install concept only: a non-AgentBundle
        // artifact must come back all-None even though its bytes would hash
        // fine. Built through the real store so the kind guard in
        // `compute_federation_digests` is what's under test — asserting on
        // `FederationDigests::none()` would pass even if the guard were removed.
        let temp = tempfile::tempdir().unwrap();
        let gateway_dir = temp.path().join(".gateway");
        std::fs::create_dir_all(&gateway_dir).unwrap();

        let (store, bundle) = build_artifact_of_kind(&gateway_dir, ArtifactKind::Dataset);
        let digests = compute_federation_digests(&bundle, &store);
        assert!(digests.code_digest.is_none(), "{digests:?}");
        assert!(digests.contract_digest.is_none(), "{digests:?}");
        assert!(digests.prose_digest.is_none(), "{digests:?}");
    }

    #[test]
    fn federation_digests_present_for_agent_bundle() {
        // Positive control for the guard above: the identical bytes under
        // `AgentBundle` must produce all three digests. Without this, the
        // None-assertion could pass for the wrong reason (e.g. an unreadable
        // store).
        let temp = tempfile::tempdir().unwrap();
        let gateway_dir = temp.path().join(".gateway");
        std::fs::create_dir_all(&gateway_dir).unwrap();

        let (store, bundle) = build_artifact_of_kind(&gateway_dir, ArtifactKind::AgentBundle);
        let digests = compute_federation_digests(&bundle, &store);
        assert!(digests.code_digest.is_some(), "{digests:?}");
        assert!(digests.contract_digest.is_some(), "{digests:?}");
        assert!(digests.prose_digest.is_some(), "{digests:?}");
    }

    #[test]
    fn declared_entrypoint_is_excluded_from_prose_digest() {
        // code and prose are complements. A declared entrypoint with a
        // non-standard extension is code, so editing it must move only the code
        // digest — if it also landed in prose, a code-only fix would void
        // static_evaluator for nothing.
        let entrypoints = vec!["agent.rules".to_string()];
        let files_a = hashing_input(&[("agent.rules", "A"), ("README.md", "docs")]);
        let files_b = hashing_input(&[("agent.rules", "B"), ("README.md", "docs")]);

        let (_, prose_a) = compute_contract_and_prose_digests(&files_a, &entrypoints);
        let (_, prose_b) = compute_contract_and_prose_digests(&files_b, &entrypoints);
        assert_eq!(
            prose_a, prose_b,
            "an entrypoint edit must not move the prose digest"
        );

        let code_a = compute_code_digest(&files_a, &entrypoints).unwrap();
        let code_b = compute_code_digest(&files_b, &entrypoints).unwrap();
        assert_ne!(code_a, code_b, "an entrypoint edit must move the code digest");
    }

    #[test]
    fn runtime_lock_is_code_not_prose() {
        // The pinned closure feeds unit_test_runner's imports; a dependency
        // change must void the code gates, not read as a prose edit.
        assert!(is_code_file("runtime.lock", &[]));
    }

    #[test]
    fn contract_digest_changes_on_security_relevant_field_flips() {
        // Each of these gates something a code-reviewing gate verifies against,
        // so flipping it must move the contract digest. `open_web` (wildcard
        // NetworkAccess) and the tool-surface lists were missing from the table
        // originally — a silent-bypass hole.
        for (before, after, field) in [
            ("open_web: false", "open_web: true", "open_web"),
            (
                "allowed_tool_tiers: [core]",
                "allowed_tool_tiers: [core, privileged]",
                "allowed_tool_tiers",
            ),
            (
                "excluded_tools: [sandbox_exec]",
                "excluded_tools: []",
                "excluded_tools",
            ),
            (
                "runtime:\n    sandbox: bubblewrap",
                "runtime:\n    sandbox: docker",
                "runtime.sandbox",
            ),
        ] {
            let a: serde_yaml::Value =
                serde_yaml::from_str(&format!("autonoetic:\n  {before}\n")).unwrap();
            let b: serde_yaml::Value =
                serde_yaml::from_str(&format!("autonoetic:\n  {after}\n")).unwrap();
            assert_ne!(
                canonical_contract_digest(&a),
                canonical_contract_digest(&b),
                "changing `{field}` must move the contract digest"
            );
        }
    }

    #[test]
    fn contract_digest_ignores_gateway_autofilled_runtime_versions() {
        // `runtime` is covered by nested paths, not wholesale: hashing the
        // gateway-autofilled version fields would void every carried verdict on
        // an unrelated gateway upgrade.
        let a: serde_yaml::Value = serde_yaml::from_str(
            "autonoetic:\n  runtime:\n    engine: autonoetic\n    gateway_version: \"0.1.0\"\n    sdk_version: \"0.1.0\"\n    sandbox: bubblewrap\n",
        )
        .unwrap();
        let b: serde_yaml::Value = serde_yaml::from_str(
            "autonoetic:\n  runtime:\n    engine: autonoetic\n    gateway_version: \"0.2.0\"\n    sdk_version: \"0.2.0\"\n    sandbox: bubblewrap\n",
        )
        .unwrap();
        assert_eq!(
            canonical_contract_digest(&a),
            canonical_contract_digest(&b),
            "a gateway version bump must not void carried verdicts"
        );
    }

    // --- Stage 2: artifact diff computation tests ---
    //
    // These build two real agent-bundle artifacts via ArtifactStore and diff
    // them, exercising the full compute_artifact_diff path.

    use crate::runtime::content_store::ContentStore;
    use autonoetic_types::artifact::ArtifactKind;
    use tempfile::tempdir;

    /// Build an agent-bundle artifact from the given (name, content) file set.
    fn build_bundle(
        gateway_dir: &std::path::Path,
        session_id: &str,
        files: &[(&str, &str)],
        entrypoints: &[&str],
    ) -> autonoetic_types::artifact::ArtifactBundle {
        let content_store = ContentStore::new(gateway_dir).unwrap();
        let mut input_names = Vec::new();
        for (name, content) in files {
            let handle = content_store.write(content.as_bytes()).unwrap();
            content_store
                .register_name(session_id, name, &handle)
                .unwrap();
            input_names.push(name.to_string());
        }
        let artifact_store = crate::artifact_store::ArtifactStore::new(gateway_dir).unwrap();
        artifact_store
            .build_with_kind(
                &input_names,
                Some(
                    &entrypoints
                        .iter()
                        .map(|s| s.to_string())
                        .collect::<Vec<_>>(),
                ),
                None,
                ArtifactKind::AgentBundle,
                session_id,
            )
            .unwrap()
    }

    fn skill_md(body: &str, capabilities_yaml: &str) -> String {
        format!(
            "---\nmetadata:\n  autonoetic:\n    agent_id: test-agent\n    capabilities:\n{}\n---\n{}",
            capabilities_yaml, body
        )
    }

    #[test]
    fn diff_prose_only_change_keeps_code_gates_eligible() {
        // The whole point: a SKILL.md body edit doesn't move the code or
        // contract digest, so unit_test_runner / auditor / sealed_evaluator
        // are carry-eligible. static_evaluator is NOT (it reviews prose).
        let temp = tempdir().unwrap();
        let gw = temp.path().join(".gateway");
        std::fs::create_dir_all(&gw).unwrap();

        let caps = "      - type: CodeExecution\n        patterns:\n          - python*\n";
        let v1 = build_bundle(
            &gw,
            "s1",
            &[
                ("main.py", "print('hi')"),
                ("SKILL.md", &skill_md("body A", caps)),
            ],
            &["main.py"],
        );
        let v2 = build_bundle(
            &gw,
            "s1",
            &[
                ("main.py", "print('hi')"),
                ("SKILL.md", &skill_md("body B", caps)),
            ],
            &["main.py"],
        );

        let store = crate::artifact_store::ArtifactStore::new(&gw).unwrap();
        let diff = compute_artifact_diff(&v1, &v2, &store).unwrap();

        assert!(!diff.code_changed, "code digest must be stable");
        assert!(!diff.contract_changed, "contract digest must be stable");
        assert!(diff.prose_changed, "prose digest must move on body edit");
        assert!(
            diff.carry_eligible_roles.contains(&"unit_test_runner"),
            "unit_test_runner should be carry-eligible on a prose-only change"
        );
        assert!(
            diff.carry_eligible_roles.contains(&"auditor"),
            "auditor should be carry-eligible on a prose-only change"
        );
        assert!(
            !diff.carry_eligible_roles.contains(&"static_evaluator"),
            "static_evaluator must NOT be carry-eligible when prose changed"
        );
        // File-level diff should show SKILL.md modified.
        assert_eq!(diff.changed_files.len(), 1);
        assert_eq!(diff.changed_files[0].file, "SKILL.md");
    }

    #[test]
    fn diff_code_change_voids_all_code_gates() {
        // A code edit trips code_digest → no code-gate carry, including
        // static_evaluator.
        let temp = tempdir().unwrap();
        let gw = temp.path().join(".gateway");
        std::fs::create_dir_all(&gw).unwrap();

        let caps = "      - type: CodeExecution\n        patterns:\n          - python*\n";
        let v1 = build_bundle(
            &gw,
            "s1",
            &[
                ("main.py", "print('a')"),
                ("SKILL.md", &skill_md("body", caps)),
            ],
            &["main.py"],
        );
        let v2 = build_bundle(
            &gw,
            "s1",
            &[
                ("main.py", "print('b')"),
                ("SKILL.md", &skill_md("body", caps)),
            ],
            &["main.py"],
        );

        let store = crate::artifact_store::ArtifactStore::new(&gw).unwrap();
        let diff = compute_artifact_diff(&v1, &v2, &store).unwrap();

        assert!(diff.code_changed);
        assert!(diff.carry_eligible_roles.is_empty());
    }

    #[test]
    fn diff_contract_change_voids_all_gates() {
        // A capabilities change is a contract change → nothing carries.
        let temp = tempdir().unwrap();
        let gw = temp.path().join(".gateway");
        std::fs::create_dir_all(&gw).unwrap();

        let caps_before = "      - type: CodeExecution\n        patterns:\n          - python*\n";
        let caps_after =
            "      - type: CodeExecution\n        patterns:\n          - python*\n      - type: SandboxFunctions\n        allowed:\n          - content.\n";
        let v1 = build_bundle(
            &gw,
            "s1",
            &[
                ("main.py", "print('hi')"),
                ("SKILL.md", &skill_md("body", caps_before)),
            ],
            &["main.py"],
        );
        let v2 = build_bundle(
            &gw,
            "s1",
            &[
                ("main.py", "print('hi')"),
                ("SKILL.md", &skill_md("body", caps_after)),
            ],
            &["main.py"],
        );

        let store = crate::artifact_store::ArtifactStore::new(&gw).unwrap();
        let diff = compute_artifact_diff(&v1, &v2, &store).unwrap();

        assert!(!diff.code_changed);
        assert!(diff.contract_changed, "capabilities change is a contract change");
        assert!(diff.carry_eligible_roles.is_empty());
    }

    #[test]
    fn diff_frontmatter_shape_change_does_not_void_verdicts() {
        // Switching between the two accepted frontmatter shapes (top-level
        // autonoetic: vs metadata.autonoetic:) with no semantic change must
        // NOT trip the contract digest — that's the canonicalization guarantee.
        let temp = tempdir().unwrap();
        let gw = temp.path().join(".gateway");
        std::fs::create_dir_all(&gw).unwrap();

        let skill_top = "---\nautonoetic:\n  agent_id: test-agent\n  capabilities:\n    - type: CodeExecution\n      patterns:\n        - python*\n---\nbody\n";
        let skill_meta = "---\nmetadata:\n  autonoetic:\n    agent_id: test-agent\n    capabilities:\n      - type: CodeExecution\n        patterns:\n          - python*\n---\nbody\n";
        let v1 = build_bundle(
            &gw,
            "s1",
            &[("main.py", "print('hi')"), ("SKILL.md", skill_top)],
            &["main.py"],
        );
        let v2 = build_bundle(
            &gw,
            "s1",
            &[("main.py", "print('hi')"), ("SKILL.md", skill_meta)],
            &["main.py"],
        );

        let store = crate::artifact_store::ArtifactStore::new(&gw).unwrap();
        let diff = compute_artifact_diff(&v1, &v2, &store).unwrap();

        // Same bytes everywhere (the SKILL.md content differs in shape only),
        // so prose_changed will be true (the file content differs), but the
        // contract digest must be identical across shapes.
        assert!(!diff.contract_changed, "shape-only change must not void contract");
        assert!(!diff.code_changed);
    }

    // --- Stage 3: verify_carry_claim tests ---

    use autonoetic_types::config::CarryForwardStrictness;
    use autonoetic_types::promotion::{Finding, FindingSeverity, PromotionRecord};

    fn finding() -> Finding {
        Finding {
            severity: FindingSeverity::Info,
            description: "ok".to_string(),
            evidence: None,
        }
    }

    /// A PromotionRecord with a passing auditor verdict and the given digests.
    fn prior_record_with_auditor_pass(code: &str, contract: &str) -> PromotionRecord {
        PromotionRecord {
            artifact_id: "art_prior".to_string(),
            artifact_digest: None,
            content_digest: None,
            evaluator_id: None,
            evaluator_pass: false,
            evaluator_findings: vec![],
            evaluator_timestamp: None,
            evaluator_execution_trace_id: None,
            auditor_id: Some("auditor.default".to_string()),
            auditor_pass: true,
            auditor_findings: vec![finding()],
            auditor_timestamp: Some("2026-01-01T00:00:00Z".to_string()),
            static_evaluator_id: None,
            static_evaluator_pass: false,
            static_evaluator_findings: vec![],
            static_evaluator_timestamp: None,
            static_evaluator_execution_trace_id: None,
            unit_test_runner_id: None,
            unit_test_runner_pass: false,
            unit_test_runner_findings: vec![],
            unit_test_runner_timestamp: None,
            unit_test_runner_execution_trace_id: None,
            sealed_evaluator_id: None,
            sealed_evaluator_pass: false,
            sealed_evaluator_findings: vec![],
            sealed_evaluator_timestamp: None,
            sealed_evaluator_execution_trace_id: None,
            promotion_gate_version: "2.2".to_string(),
            blessed_packages: vec![],
            code_digest: Some(code.to_string()),
            contract_digest: Some(contract.to_string()),
            prose_digest: Some("sha256:prose-prior".to_string()),
            carried_roles: std::collections::BTreeMap::new(),
        }
    }

    fn current_digests(code: &str, contract: &str) -> FederationDigests {
        FederationDigests {
            code_digest: Some(code.to_string()),
            contract_digest: Some(contract.to_string()),
            prose_digest: Some("sha256:prose-current".to_string()),
        }
    }

    #[test]
    fn carry_accepted_when_digests_match_and_strictness_conservative() {
        let prior = prior_record_with_auditor_pass("sha256:code-x", "sha256:contract-y");
        let cur = current_digests("sha256:code-x", "sha256:contract-y");
        assert!(
            verify_carry_claim(
                &PromotionRole::Auditor,
                Some(&prior),
                &cur,
                CarryForwardStrictness::Conservative,
            )
            .is_ok()
        );
    }

    #[test]
    fn carry_rejected_when_strictness_off() {
        // Even with matching digests, `off` disallows any carry.
        let prior = prior_record_with_auditor_pass("sha256:code-x", "sha256:contract-y");
        let cur = current_digests("sha256:code-x", "sha256:contract-y");
        assert_eq!(
            verify_carry_claim(
                &PromotionRole::Auditor,
                Some(&prior),
                &cur,
                CarryForwardStrictness::Off,
            ),
            Err(CarryRejection::StrictnessDisallowed)
        );
    }

    #[test]
    fn carry_rejected_when_code_digest_differs() {
        let prior = prior_record_with_auditor_pass("sha256:code-x", "sha256:contract-y");
        let cur = current_digests("sha256:code-DIFFERENT", "sha256:contract-y");
        assert_eq!(
            verify_carry_claim(
                &PromotionRole::Auditor,
                Some(&prior),
                &cur,
                CarryForwardStrictness::Conservative,
            ),
            Err(CarryRejection::DigestMismatch { field: "code_digest" })
        );
    }

    #[test]
    fn carry_rejected_when_contract_digest_differs() {
        let prior = prior_record_with_auditor_pass("sha256:code-x", "sha256:contract-y");
        let cur = current_digests("sha256:code-x", "sha256:contract-DIFFERENT");
        assert_eq!(
            verify_carry_claim(
                &PromotionRole::Auditor,
                Some(&prior),
                &cur,
                CarryForwardStrictness::Conservative,
            ),
            Err(CarryRejection::DigestMismatch { field: "contract_digest" })
        );
    }

    #[test]
    fn carry_rejected_when_no_prior_record() {
        let cur = current_digests("sha256:code-x", "sha256:contract-y");
        assert_eq!(
            verify_carry_claim(
                &PromotionRole::Auditor,
                None,
                &cur,
                CarryForwardStrictness::Conservative,
            ),
            Err(CarryRejection::PriorRecordMissing)
        );
    }

    #[test]
    fn carry_rejected_when_prior_role_did_not_pass() {
        let mut prior = prior_record_with_auditor_pass("sha256:code-x", "sha256:contract-y");
        prior.auditor_pass = false;
        let cur = current_digests("sha256:code-x", "sha256:contract-y");
        assert_eq!(
            verify_carry_claim(
                &PromotionRole::Auditor,
                Some(&prior),
                &cur,
                CarryForwardStrictness::Conservative,
            ),
            Err(CarryRejection::PriorVerdictNotPassed)
        );
    }

    #[test]
    fn carry_rejected_when_role_is_static_evaluator() {
        // static_evaluator reviews prose → never carry-eligible.
        let prior = prior_record_with_auditor_pass("sha256:code-x", "sha256:contract-y");
        let cur = current_digests("sha256:code-x", "sha256:contract-y");
        assert_eq!(
            verify_carry_claim(
                &PromotionRole::StaticEvaluator,
                Some(&prior),
                &cur,
                CarryForwardStrictness::Conservative,
            ),
            Err(CarryRejection::RoleNotCarryEligible)
        );
    }

    #[test]
    fn carry_rejected_when_prior_record_lacks_digests() {
        // A legacy record (pre-feature, None digests) is unverifiable.
        let mut prior = prior_record_with_auditor_pass("sha256:code-x", "sha256:contract-y");
        prior.code_digest = None;
        prior.contract_digest = None;
        let cur = current_digests("sha256:code-x", "sha256:contract-y");
        assert_eq!(
            verify_carry_claim(
                &PromotionRole::Auditor,
                Some(&prior),
                &cur,
                CarryForwardStrictness::Conservative,
            ),
            Err(CarryRejection::PriorRecordMissing)
        );
    }
}
