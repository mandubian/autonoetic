//! Semantic summaries for reconciled workbench diffs.
//!
//! Issue #332: when an operator returns a workbench to the orchestrator,
//! the wake-up payload should carry both the raw diff (file lists) and a
//! higher-level orientation that calls out potential agent I/O contract
//! changes (capability additions/removals, skill manifest edits, runtime
//! lock changes, etc.). This module defines the data types and the trait
//! used to produce those summaries.
//!
//! The default implementation (in `autonoetic-gateway/src/runtime/semantic_diff.rs`)
//! is a rule-based classifier keyed on file path and content patterns. It
//! is deliberately pluggable: any type that implements
//! [`SemanticSummarizer`] can be wired in later (e.g. an LLM-based
//! summarizer behind a feature flag) without changing the
//! reconcile → `/return` wake-up contract.
//!
//! Design constraints:
//!
//! - No new LLM infrastructure. The default impl does not call a model.
//! - The summary is **additive** to the raw diff, never a replacement.
//!   The orchestrator should be able to ground on the raw file lists if
//!   the summary is wrong or stale.
//! - The summary is persisted next to `reconciliation.json` so it can be
//!   reread by `/return` and by audit consumers.

use serde::{Deserialize, Serialize};

use crate::plan_frame::PlanFrameSummary;
use crate::workbench::{FileChangeType, WorkbenchFileDiff};

/// The contract-impact classification of a single file change.
///
/// The variants are ordered roughly from highest blast-radius to lowest.
/// `None` is the default for files that don't match any known contract.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum ContractImpact {
    /// No contract impact (regular source / doc change).
    None,
    /// A capability declaration file was changed. The agent may have
    /// gained or lost capabilities.
    CapabilityChange,
    /// A skill manifest was added / modified / removed. The orchestrator
    /// may need to re-resolve agent routing.
    SkillManifestChange,
    /// The runtime lock was changed. The orchestrator may need to
    /// re-evaluate which decisions were inherited.
    RuntimeLockChange,
    /// A config schema or template was changed. Other artifacts depending
    /// on this config may need to be re-validated.
    ConfigSchemaChange,
    /// An entry point (main.rs, agent.toml) was changed.
    EntryPointChange,
    /// Code matching the remote-access / network patterns was changed.
    /// The orchestrator may need to re-issue approvals.
    NetworkAccessChange,
    /// A credential-shape file was changed (the file is referenced from
    /// the credential vault, not that the secret is in the diff).
    CredentialShapeChange,
    /// The file is in a directory that *could* be a contract but doesn't
    /// match any specific known shape. Surfaced for manual review.
    UnknownContract,
}

impl ContractImpact {
    pub fn as_str(&self) -> &'static str {
        match self {
            ContractImpact::None => "none",
            ContractImpact::CapabilityChange => "capability_change",
            ContractImpact::SkillManifestChange => "skill_manifest_change",
            ContractImpact::RuntimeLockChange => "runtime_lock_change",
            ContractImpact::ConfigSchemaChange => "config_schema_change",
            ContractImpact::EntryPointChange => "entry_point_change",
            ContractImpact::NetworkAccessChange => "network_access_change",
            ContractImpact::CredentialShapeChange => "credential_shape_change",
            ContractImpact::UnknownContract => "unknown_contract",
        }
    }
}

/// The role of a file in the workbench. Used to feed the
/// `ContractImpact` classifier and to label each entry in
/// `file_classifications` so consumers can group by role.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum FileRole {
    Capability,
    SkillManifest,
    RuntimeLock,
    ConfigSchema,
    EntryPoint,
    NetworkAccess,
    Credential,
    SourceCode,
    Documentation,
    Build,
    Test,
    Unknown,
}

impl FileRole {
    pub fn as_str(&self) -> &'static str {
        match self {
            FileRole::Capability => "capability",
            FileRole::SkillManifest => "skill_manifest",
            FileRole::RuntimeLock => "runtime_lock",
            FileRole::ConfigSchema => "config_schema",
            FileRole::EntryPoint => "entry_point",
            FileRole::NetworkAccess => "network_access",
            FileRole::Credential => "credential",
            FileRole::SourceCode => "source_code",
            FileRole::Documentation => "documentation",
            FileRole::Build => "build",
            FileRole::Test => "test",
            FileRole::Unknown => "unknown",
        }
    }
}

/// One per-file classification. Stable, ordered by path, mirrors
/// `WorkbenchFileDiff` 1:1 (filtered to non-unchanged entries).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FileClassification {
    pub path: String,
    pub change_type: FileChangeType,
    pub role: FileRole,
    pub impact: ContractImpact,
    /// Short human-readable rationale, e.g. "matches capabilities.yaml".
    /// Empty string when the classifier has nothing useful to say.
    pub rationale: String,
}

/// One detected contract-level change. This is a filtered view of
/// `FileClassification` keeping only entries where `impact != None`.
///
/// The list is sorted by `impact` then `path` for deterministic output.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ContractChange {
    pub path: String,
    pub change_type: FileChangeType,
    pub impact: ContractImpact,
    pub rationale: String,
}

/// Validation state at reconcile time, derived from the gateway store
/// (waivers on the base artifact) and the active plan (required
/// validations). The summary reports *what the orchestrator should know*
/// about validation; it does not produce validation findings.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct ValidationState {
    /// Total waivers on the base artifact.
    pub waiver_count: usize,
    /// Whether at least one waiver is present. Mirrors `waiver_count > 0`
    /// for ergonomic JSON consumers.
    pub waivers_present: bool,
    /// Validation ids the active plan marked as required.
    pub required_validations: Vec<String>,
    /// Validation ids the active plan marked as advisory.
    pub advisory_validations: Vec<String>,
    /// Validation ids required by the plan that are *not* covered by a
    /// waiver. Empty when the plan is absent or all required validations
    /// are satisfied/waived.
    pub unsatisfied_required: Vec<String>,
}

/// Top-level semantic summary persisted in
/// `<workbench>/.autonoetic/semantic_summary.json` and inlined into the
/// `/return` wake-up metadata.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SemanticSummary {
    pub workbench_id: String,
    pub base_artifact_id: String,
    pub new_artifact_id: String,
    pub plan_id: Option<String>,
    pub plan_version: Option<u32>,
    pub total_files: usize,
    pub changed_files: usize,
    pub added_files: usize,
    pub modified_files: usize,
    pub deleted_files: usize,
    /// Only entries where `impact != None`. Sorted by impact then path.
    pub contract_changes: Vec<ContractChange>,
    /// All non-unchanged entries classified by role. Sorted by path.
    pub file_classifications: Vec<FileClassification>,
    pub validation_state: ValidationState,
    /// Implementation id, e.g. `rule_based_v1`. Free-form so future
    /// impls (LLM-based, etc.) can self-describe.
    pub summarizer_id: String,
    pub generated_at: String,
}

/// Inputs the summarizer needs. Held by reference so the gateway can
/// reuse file buffers it already loaded.
pub struct SemanticSummaryInputs<'a> {
    pub workbench_id: &'a str,
    pub base_artifact_id: &'a str,
    pub new_artifact_id: &'a str,
    pub diffs: &'a [WorkbenchFileDiff],
    /// Map from relative path → file content for the current (new)
    /// version of each file. Populated for Added and Modified entries;
    /// empty for Deleted entries (use `base_files` instead).
    pub current_files: &'a std::collections::HashMap<String, Vec<u8>>,
    /// Map from relative path → file content for the base (original)
    /// version of each file. Populated for Modified and Deleted entries
    /// so the classifier can inspect the *old* content (e.g. detect
    /// network-access patterns in a deleted source file).
    pub base_files: &'a std::collections::HashMap<String, Vec<u8>>,
    pub plan: Option<&'a PlanFrameSummary>,
    /// Waiver ids on the base artifact, indexed by validation id.
    pub waivers_by_validation: &'a std::collections::HashMap<String, Vec<String>>,
    pub generated_at: &'a str,
}

/// Pluggable summarizer trait. Implementations must be deterministic
/// (no wall-clock-dependent ordering) so persisted summaries can be
/// re-derived and compared.
pub trait SemanticSummarizer: Send + Sync {
    /// Stable id used in [`SemanticSummary::summarizer_id`].
    fn id(&self) -> &'static str;

    /// Build a [`SemanticSummary`] from the given inputs. Implementations
    /// should treat empty / missing optional fields as legitimate and
    /// return `None` for any derived value they cannot produce, rather
    /// than erroring.
    fn summarize(&self, inputs: &SemanticSummaryInputs<'_>) -> SemanticSummary;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contract_impact_serializes_snake_case() {
        assert_eq!(
            serde_json::to_string(&ContractImpact::CapabilityChange).unwrap(),
            "\"capability_change\""
        );
        assert_eq!(
            serde_json::to_string(&ContractImpact::NetworkAccessChange).unwrap(),
            "\"network_access_change\""
        );
    }

    #[test]
    fn file_role_serializes_snake_case() {
        assert_eq!(
            serde_json::to_string(&FileRole::SkillManifest).unwrap(),
            "\"skill_manifest\""
        );
    }

    #[test]
    fn semantic_summary_round_trip_empty() {
        let s = SemanticSummary {
            workbench_id: "wb-x".into(),
            base_artifact_id: "ar.abc".into(),
            new_artifact_id: "ar.def".into(),
            plan_id: None,
            plan_version: None,
            total_files: 0,
            changed_files: 0,
            added_files: 0,
            modified_files: 0,
            deleted_files: 0,
            contract_changes: vec![],
            file_classifications: vec![],
            validation_state: ValidationState::default(),
            summarizer_id: "test".into(),
            generated_at: "2026-06-01T00:00:00Z".into(),
        };
        let json = serde_json::to_string(&s).unwrap();
        let back: SemanticSummary = serde_json::from_str(&json).unwrap();
        assert_eq!(s, back);
    }
}
