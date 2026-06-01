//! Rule-based [`SemanticSummarizer`] implementation.
//!
//! This is the default impl for issue #332. It is deliberately
//! deterministic, no-LLM, and **pluggable** — swap the `summarizer`
//! field on a `WorkbenchReconcileTool` call site (or any future wiring
//! point) to use a different impl without touching the rest of the
//! pipeline.
//!
//! ## Classification rules
//!
//! Each non-unchanged diff entry is classified into a [`FileRole`]
//! based on path and content:
//!
//! - `capability`: filename is `capabilities.yaml`, `capabilities.yml`,
//!   `agent.toml`, or path segment `capabilities/`, `agents/<id>/capability*`.
//! - `skill_manifest`: filename is `SKILL.md` or path segment `skills/`.
//! - `runtime_lock`: filename is `runtime_lock.json` or in `.autonoetic/`.
//! - `config_schema`: filename is `config-template.yaml` or path
//!   segment `config/`, `schemas/`.
//! - `entry_point`: filename is `main.rs`, `lib.rs`, `main.py`, `__init__.py`,
//!   `mod.rs`, `agent.toml` (also capability), or path segment `bin/`,
//!   `src/bin/`, `agents/<id>/agent.toml`.
//! - `network_access`: file content matches the
//!   `RemoteAccessAnalyzer::analyze_code` patterns. Only applied to
//!   source-code files to avoid false positives on config schemas.
//! - `credential`: filename is `credentials.yaml`, `*.pem`, `*.key`,
//!   `*.crt`, `*.p12`, `*.pfx`, or path segment `secrets/`.
//! - `documentation`: `*.md`.
//! - `test`: path segment `tests/`, `_test.rs`, `_test.py`, `test_*.py`,
//!   `*.test.ts`, `*.spec.ts`.
//! - `build`: filename is `Cargo.toml`, `Cargo.lock`, `package.json`,
//!   `pyproject.toml`, `poetry.lock`, `requirements.txt`.
//! - `source_code`: extension `*.rs`, `*.py`, `*.ts`, `*.tsx`, `*.js`,
//!   `*.go`, `*.java`, `*.c`, `*.cpp`, `*.h`, `*.hpp`, `*.rb`, `*.sh`.
//! - `unknown`: anything else.
//!
//! ## Contract-impact mapping
//!
//! Each role maps to a [`ContractImpact`]:
//!
//! - `Capability`         → `CapabilityChange`
//! - `SkillManifest`      → `SkillManifestChange`
//! - `RuntimeLock`        → `RuntimeLockChange`
//! - `ConfigSchema`       → `ConfigSchemaChange`
//! - `EntryPoint`         → `EntryPointChange`
//! - `NetworkAccess`      → `NetworkAccessChange`
//! - `Credential`         → `CredentialShapeChange`
//! - `SourceCode`,
//!   `Test`,
//!   `Documentation`,
//!   `Build`,
//!   `Unknown`            → `None`
//!
//! ## Validation state
//!
//! - `waiver_count` and `waivers_present` mirror the inputs.
//! - `required_validations` / `advisory_validations` come from the
//!   active plan (empty when no plan is active).
//! - `unsatisfied_required` is the set of `required_validations` not
//!   covered by a waiver, sorted lexicographically. The orchestrator
//!   uses this to know which validations are still owed.

use std::collections::HashMap;

use autonoetic_types::plan_frame::PlanFrameSummary;
use autonoetic_types::semantic_diff::{
    ContractChange, ContractImpact, FileClassification, FileRole, SemanticSummarizer,
    SemanticSummary, SemanticSummaryInputs, ValidationState,
};
use autonoetic_types::workbench::{FileChangeType, WorkbenchFileDiff};

use crate::runtime::remote_access::RemoteAccessAnalyzer;

const ID: &str = "rule_based_v1";

/// Rule-based semantic summarizer. Stateless and `Clone`-cheap (one
/// small `Vec<String>` of extension sets), so callers can hold an
/// instance in a `static` and pass it everywhere.
#[derive(Debug, Clone)]
pub struct RuleBasedSemanticSummarizer {
    /// Source-code extensions eligible for the network-access check.
    /// Defaults to a sensible cross-language set; override to narrow
    /// the check.
    pub network_check_extensions: Vec<String>,
}

impl Default for RuleBasedSemanticSummarizer {
    fn default() -> Self {
        Self {
            network_check_extensions: vec![
                "rs".into(),
                "py".into(),
                "ts".into(),
                "tsx".into(),
                "js".into(),
                "go".into(),
                "java".into(),
                "c".into(),
                "cpp".into(),
                "h".into(),
                "hpp".into(),
                "rb".into(),
                "sh".into(),
            ],
        }
    }
}

impl SemanticSummarizer for RuleBasedSemanticSummarizer {
    fn id(&self) -> &'static str {
        ID
    }

    fn summarize(&self, inputs: &SemanticSummaryInputs<'_>) -> SemanticSummary {
        let mut file_classifications: Vec<FileClassification> = Vec::new();

        let mut added = 0usize;
        let mut modified = 0usize;
        let mut deleted = 0usize;
        for d in inputs.diffs {
            match d.change_type {
                FileChangeType::Added => added += 1,
                FileChangeType::Modified => modified += 1,
                FileChangeType::Deleted => deleted += 1,
                FileChangeType::Unchanged => continue,
            }

            let content = inputs.current_files.get(&d.path);
            let role = classify_role(&d.path, content, &self.network_check_extensions);
            let impact = role_to_impact(role);
            let rationale = rationale_for(&d.path, role, impact, content);

            file_classifications.push(FileClassification {
                path: d.path.clone(),
                change_type: d.change_type,
                role,
                impact,
                rationale,
            });
        }

        file_classifications.sort_by(|a, b| a.path.cmp(&b.path));

        let mut contract_changes: Vec<ContractChange> = file_classifications
            .iter()
            .filter(|c| c.impact != ContractImpact::None)
            .map(|c| ContractChange {
                path: c.path.clone(),
                change_type: c.change_type,
                impact: c.impact,
                rationale: c.rationale.clone(),
            })
            .collect();
        contract_changes.sort_by(|a, b| {
            impact_rank(b.impact)
                .cmp(&impact_rank(a.impact))
                .then_with(|| a.path.cmp(&b.path))
        });

        let validation_state =
            build_validation_state(inputs.plan, inputs.waivers_by_validation);

        let total_files = inputs.diffs.len();
        let changed_files = added + modified + deleted;

        SemanticSummary {
            workbench_id: inputs.workbench_id.to_string(),
            base_artifact_id: inputs.base_artifact_id.to_string(),
            new_artifact_id: inputs.new_artifact_id.to_string(),
            plan_id: inputs.plan.map(|p| p.plan_id.clone()),
            plan_version: inputs.plan.map(|p| p.version),
            total_files,
            changed_files,
            added_files: added,
            modified_files: modified,
            deleted_files: deleted,
            contract_changes,
            file_classifications,
            validation_state,
            summarizer_id: self.id().to_string(),
            generated_at: inputs.generated_at.to_string(),
        }
    }
}

fn classify_role(
    path: &str,
    content: Option<&Vec<u8>>,
    network_check_extensions: &[String],
) -> FileRole {
    let normalized = path.replace('\\', "/");
    let name = normalized
        .rsplit('/')
        .next()
        .unwrap_or(&normalized)
        .to_ascii_lowercase();
    let lower = normalized.to_ascii_lowercase();
    let ext = extension(&name).unwrap_or("");

    // Capability: explicit filenames / directories.
    if name == "capabilities.yaml"
        || name == "capabilities.yml"
        || name == "capabilities.json"
        || name == "agent.toml"
        || lower.contains("/capabilities/")
        || lower.contains("/capability/")
    {
        return FileRole::Capability;
    }

    // Skill manifest: SKILL.md at any depth, or `skills/` directory.
    if name == "skill.md" || lower.contains("/skills/") {
        return FileRole::SkillManifest;
    }

    // Runtime lock: explicit filename or under .autonoetic/.
    if name == "runtime_lock.json" || lower.contains("/.autonoetic/") {
        return FileRole::RuntimeLock;
    }

    // Config schema: well-known template / schemas directory.
    if name == "config-template.yaml"
        || name == "config-template.yml"
        || name == "config_schema.json"
        || lower.contains("/schemas/")
        || lower.contains("/config/")
    {
        return FileRole::ConfigSchema;
    }

    // Credential: well-known secret-shape filenames.
    if name == "credentials.yaml"
        || name == "credentials.yml"
        || name == "credentials.json"
        || ext == "pem"
        || ext == "key"
        || ext == "crt"
        || ext == "p12"
        || ext == "pfx"
        || lower.contains("/secrets/")
    {
        return FileRole::Credential;
    }

    // Entry point. `lib.rs`/`mod.rs` are library/module roots, not
    // contract boundaries on their own; only binary entry points and
    // agent manifests count.
    if name == "main.rs"
        || name == "main.py"
        || name == "__init__.py"
        || name == "agent.toml"
        || lower.contains("/bin/")
    {
        return FileRole::EntryPoint;
    }

    // Test.
    if lower.contains("/tests/")
        || name.starts_with("test_")
        || name.ends_with("_test.rs")
        || name.ends_with("_test.py")
        || name.ends_with(".test.ts")
        || name.ends_with(".spec.ts")
        || lower.contains("/__tests__/")
    {
        return FileRole::Test;
    }

    // Build.
    if name == "cargo.toml"
        || name == "cargo.lock"
        || name == "package.json"
        || name == "package-lock.json"
        || name == "pyproject.toml"
        || name == "poetry.lock"
        || name == "requirements.txt"
    {
        return FileRole::Build;
    }

    // Documentation.
    if ext == "md" || ext == "rst" || ext == "adoc" {
        return FileRole::Documentation;
    }

    // Source code → run the network-access check.
    if network_check_extensions.iter().any(|e| e == ext) {
        if let Some(bytes) = content {
            if let Ok(text) = std::str::from_utf8(bytes) {
                if !RemoteAccessAnalyzer::analyze_code(text).detected_patterns.is_empty() {
                    return FileRole::NetworkAccess;
                }
            }
        }
        return FileRole::SourceCode;
    }

    FileRole::Unknown
}

fn role_to_impact(role: FileRole) -> ContractImpact {
    match role {
        FileRole::Capability => ContractImpact::CapabilityChange,
        FileRole::SkillManifest => ContractImpact::SkillManifestChange,
        FileRole::RuntimeLock => ContractImpact::RuntimeLockChange,
        FileRole::ConfigSchema => ContractImpact::ConfigSchemaChange,
        FileRole::EntryPoint => ContractImpact::EntryPointChange,
        FileRole::NetworkAccess => ContractImpact::NetworkAccessChange,
        FileRole::Credential => ContractImpact::CredentialShapeChange,
        FileRole::SourceCode
        | FileRole::Test
        | FileRole::Documentation
        | FileRole::Build
        | FileRole::Unknown => ContractImpact::None,
    }
}

fn rationale_for(
    path: &str,
    role: FileRole,
    impact: ContractImpact,
    content: Option<&Vec<u8>>,
) -> String {
    if impact == ContractImpact::None {
        return String::new();
    }
    let name = path
        .rsplit('/')
        .next()
        .unwrap_or(path)
        .to_ascii_lowercase();
    match role {
        FileRole::Capability => format!("matches capability contract ({name})"),
        FileRole::SkillManifest => format!("matches skill manifest ({name})"),
        FileRole::RuntimeLock => format!("runtime lock update ({name})"),
        FileRole::ConfigSchema => format!("config schema update ({name})"),
        FileRole::EntryPoint => format!("entry point update ({name})"),
        FileRole::NetworkAccess => {
            let n = content
                .and_then(|b| std::str::from_utf8(b).ok())
                .map(RemoteAccessAnalyzer::analyze_code)
                .map(|a| a.detected_patterns.len())
                .unwrap_or(0);
            format!("source file with {n} remote-access pattern(s) ({name})")
        }
        FileRole::Credential => format!("credential-shape file ({name})"),
        _ => String::new(),
    }
}

fn impact_rank(impact: ContractImpact) -> u8 {
    match impact {
        ContractImpact::RuntimeLockChange => 9,
        ContractImpact::CapabilityChange => 8,
        ContractImpact::SkillManifestChange => 7,
        ContractImpact::EntryPointChange => 6,
        ContractImpact::NetworkAccessChange => 5,
        ContractImpact::CredentialShapeChange => 4,
        ContractImpact::ConfigSchemaChange => 3,
        ContractImpact::UnknownContract => 2,
        ContractImpact::None => 0,
    }
}

fn build_validation_state(
    plan: Option<&PlanFrameSummary>,
    waivers_by_validation: &HashMap<String, Vec<String>>,
) -> ValidationState {
    let waiver_count: usize = waivers_by_validation.values().map(|v| v.len()).sum();
    let (required, advisory, unsatisfied) = match plan {
        Some(p) => {
            let mut unsatisfied: Vec<String> = p
                .required_validations
                .iter()
                .filter(|v| !waivers_by_validation.contains_key(*v))
                .cloned()
                .collect();
            unsatisfied.sort();
            (p.required_validations.clone(), p.advisory_validations.clone(), unsatisfied)
        }
        None => (Vec::new(), Vec::new(), Vec::new()),
    };
    ValidationState {
        waiver_count,
        waivers_present: waiver_count > 0,
        required_validations: required,
        advisory_validations: advisory,
        unsatisfied_required: unsatisfied,
    }
}

fn extension(name: &str) -> Option<&str> {
    let idx = name.rfind('.')?;
    if idx == 0 || idx + 1 == name.len() {
        return None;
    }
    Some(&name[idx + 1..])
}

#[cfg(test)]
mod tests {
    use super::*;
    use autonoetic_types::plan_frame::{PlanFrameSummary, PlanStatus};
    use autonoetic_types::workbench::{FileChangeType, WorkbenchFileDiff};

    fn diff(path: &str, kind: FileChangeType) -> WorkbenchFileDiff {
        WorkbenchFileDiff {
            path: path.to_string(),
            change_type: kind,
            base_digest: None,
            current_digest: None,
        }
    }

    fn plan_with_required(required: &[&str]) -> PlanFrameSummary {
        PlanFrameSummary {
            plan_id: "plan-1".into(),
            version: 1,
            parent_version: None,
            status: PlanStatus::Approved,
            title: "test plan".into(),
            step_count: 0,
            operator_steps: vec![],
            agent_steps: vec![],
            required_validations: required.iter().map(|s| s.to_string()).collect(),
            advisory_validations: vec![],
        }
    }

    #[test]
    fn classifies_capability_yaml() {
        let summarizer = RuleBasedSemanticSummarizer::default();
        let diffs = vec![diff("capabilities.yaml", FileChangeType::Modified)];
        let mut files = HashMap::new();
        files.insert("capabilities.yaml".to_string(), b"[]".to_vec());
        let inputs = SemanticSummaryInputs {
            workbench_id: "wb-1",
            base_artifact_id: "ar.a",
            new_artifact_id: "ar.b",
            diffs: &diffs,
            current_files: &files,
            plan: None,
            waivers_by_validation: &HashMap::new(),
            generated_at: "2026-06-01T00:00:00Z",
        };
        let s = summarizer.summarize(&inputs);
        assert_eq!(s.contract_changes.len(), 1);
        assert_eq!(s.contract_changes[0].impact, ContractImpact::CapabilityChange);
        assert!(s.contract_changes[0].rationale.contains("capability contract"));
    }

    #[test]
    fn classifies_skill_manifest_and_runtime_lock_separately() {
        let summarizer = RuleBasedSemanticSummarizer::default();
        let diffs = vec![
            diff("agents/lead/SKILL.md", FileChangeType::Modified),
            diff(".autonoetic/runtime_lock.json", FileChangeType::Modified),
        ];
        let mut files = HashMap::new();
        files.insert("agents/lead/SKILL.md".to_string(), b"# skill".to_vec());
        files.insert(
            ".autonoetic/runtime_lock.json".to_string(),
            b"{}".to_vec(),
        );
        let inputs = SemanticSummaryInputs {
            workbench_id: "wb-1",
            base_artifact_id: "ar.a",
            new_artifact_id: "ar.b",
            diffs: &diffs,
            current_files: &files,
            plan: None,
            waivers_by_validation: &HashMap::new(),
            generated_at: "2026-06-01T00:00:00Z",
        };
        let s = summarizer.summarize(&inputs);
        assert_eq!(s.contract_changes.len(), 2);
        let impacts: Vec<_> = s.contract_changes.iter().map(|c| c.impact).collect();
        assert!(impacts.contains(&ContractImpact::SkillManifestChange));
        assert!(impacts.contains(&ContractImpact::RuntimeLockChange));
        // Runtime lock should be ranked first.
        assert_eq!(s.contract_changes[0].impact, ContractImpact::RuntimeLockChange);
    }

    #[test]
    fn detects_network_access_pattern_in_source_file() {
        let summarizer = RuleBasedSemanticSummarizer::default();
        let diffs = vec![diff("src/lib.rs", FileChangeType::Modified)];
        let mut files = HashMap::new();
        files.insert(
            "src/lib.rs".to_string(),
            b"fn fetch() { let _ = reqwest::get(\"https://example.com\"); }".to_vec(),
        );
        let inputs = SemanticSummaryInputs {
            workbench_id: "wb-1",
            base_artifact_id: "ar.a",
            new_artifact_id: "ar.b",
            diffs: &diffs,
            current_files: &files,
            plan: None,
            waivers_by_validation: &HashMap::new(),
            generated_at: "2026-06-01T00:00:00Z",
        };
        let s = summarizer.summarize(&inputs);
        let c = &s.file_classifications[0];
        assert_eq!(c.role, FileRole::NetworkAccess);
        assert_eq!(c.impact, ContractImpact::NetworkAccessChange);
        assert!(c.rationale.contains("remote-access pattern"));
    }

    #[test]
    fn source_without_network_pattern_is_pure_source() {
        let summarizer = RuleBasedSemanticSummarizer::default();
        let diffs = vec![diff("src/lib.rs", FileChangeType::Modified)];
        let mut files = HashMap::new();
        files.insert(
            "src/lib.rs".to_string(),
            b"fn add(a: i32, b: i32) -> i32 { a + b }".to_vec(),
        );
        let inputs = SemanticSummaryInputs {
            workbench_id: "wb-1",
            base_artifact_id: "ar.a",
            new_artifact_id: "ar.b",
            diffs: &diffs,
            current_files: &files,
            plan: None,
            waivers_by_validation: &HashMap::new(),
            generated_at: "2026-06-01T00:00:00Z",
        };
        let s = summarizer.summarize(&inputs);
        assert_eq!(s.file_classifications[0].role, FileRole::SourceCode);
        assert_eq!(s.file_classifications[0].impact, ContractImpact::None);
        assert!(s.contract_changes.is_empty());
    }

    #[test]
    fn validation_state_marks_unsatisfied_required() {
        let summarizer = RuleBasedSemanticSummarizer::default();
        let diffs = vec![diff("src/lib.rs", FileChangeType::Modified)];
        let mut files = HashMap::new();
        files.insert("src/lib.rs".to_string(), b"fn x() {}".to_vec());
        let plan = plan_with_required(&["unit_tests", "security_review", "lint"]);
        let mut waivers = HashMap::new();
        waivers.insert("unit_tests".to_string(), vec!["vw-1".to_string()]);
        let inputs = SemanticSummaryInputs {
            workbench_id: "wb-1",
            base_artifact_id: "ar.a",
            new_artifact_id: "ar.b",
            diffs: &diffs,
            current_files: &files,
            plan: Some(&plan),
            waivers_by_validation: &waivers,
            generated_at: "2026-06-01T00:00:00Z",
        };
        let s = summarizer.summarize(&inputs);
        assert_eq!(s.validation_state.waiver_count, 1);
        assert!(s.validation_state.waivers_present);
        assert_eq!(
            s.validation_state.required_validations,
            vec!["unit_tests", "security_review", "lint"]
        );
        assert_eq!(
            s.validation_state.unsatisfied_required,
            vec!["lint", "security_review"]
        );
    }

    #[test]
    fn file_classifications_omit_unchanged_entries() {
        let summarizer = RuleBasedSemanticSummarizer::default();
        let diffs = vec![
            diff("a.rs", FileChangeType::Unchanged),
            diff("b.rs", FileChangeType::Modified),
        ];
        let mut files = HashMap::new();
        files.insert("b.rs".to_string(), b"fn x() {}".to_vec());
        let inputs = SemanticSummaryInputs {
            workbench_id: "wb-1",
            base_artifact_id: "ar.a",
            new_artifact_id: "ar.b",
            diffs: &diffs,
            current_files: &files,
            plan: None,
            waivers_by_validation: &HashMap::new(),
            generated_at: "2026-06-01T00:00:00Z",
        };
        let s = summarizer.summarize(&inputs);
        assert_eq!(s.total_files, 2);
        assert_eq!(s.changed_files, 1);
        assert_eq!(s.file_classifications.len(), 1);
        assert_eq!(s.file_classifications[0].path, "b.rs");
    }
}
