//! Workbench "return to agent" helpers shared between the Chat TUI and the
//! Session Room TUI.
//!
//! Issue: the Session Room TUI is a remote JSON-RPC client of the gateway and
//! cannot read the workbench workspace files directly. The Chat TUI runs in the
//! same process as the gateway and already had this logic inline. Moving it
//! here lets both TUIs (and the gateway RPC layer) share the same return-payload
//! construction and safety checks.

use crate::scheduler::gateway_store::GatewayStore;
use autonoetic_types::semantic_diff::SemanticSummary;
use autonoetic_types::workbench::WorkbenchStatus;
use sha2::Digest;
use std::collections::HashMap;
use std::path::Path;

/// Inputs for `build_return_to_agent_wakeup`. Kept as a plain struct so the
/// builder stays unit-testable without constructing a full workbench record.
/// Owns all of its data (no borrowed references) so the caller can free
/// scratch workbench lookups after constructing the input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReturnToAgentInput {
    pub workbench_id: String,
    pub base_artifact_id: String,
    /// True when the workbench has been reconciled (i.e. the operator already
    /// committed the edits into a new artifact revision). False when the
    /// workbench is still active and the wake-up is being sent without a
    /// prior reconcile (operator chose --force).
    pub reconciled: bool,
    /// Optional new artifact ref/id from the most recent reconcile.
    pub new_artifact_ref: Option<String>,
    pub new_artifact_id: Option<String>,
    /// Optional operator note typed alongside `/return ...`.
    pub operator_note: Option<String>,
    /// Number of files that differ from the base artifact (modified+added+deleted).
    /// 0 when the workbench is in sync with the base (or already reconciled).
    pub unsaved_change_count: usize,
    /// IDs of files modified by the operator since the projection. May be
    /// empty when the workbench is reconciled or unsaved_change_count is 0.
    pub operator_modified_files: Vec<String>,
    /// IDs of files added by the operator since the projection.
    pub operator_added_files: Vec<String>,
    /// IDs of files deleted by the operator since the projection.
    pub deleted_files: Vec<String>,
    /// Issue #332: high-level semantic summary of the diff (contract
    /// impact, validation state, file-role classifications). Loaded
    /// from `.autonoetic/semantic_summary.json` for reconciled
    /// workbenches; `None` when missing or for active workbenches.
    /// Uses the typed struct so `ReturnToAgentInput` preserves `Eq`.
    pub semantic_summary: Option<SemanticSummary>,
}

/// Output of `build_return_to_agent_wakeup`. The `message` is the natural
/// language text the orchestrator will read; `metadata` is the structured
/// `workbench_reconciled` payload attached to the event.ingest call for
/// downstream tooling and the agent's own state updates.
#[derive(Debug, Clone, PartialEq)]
pub struct ReturnToAgentWakeup {
    pub message: String,
    pub metadata: serde_json::Value,
}

/// Build the orchestrator wake-up for a workbench return.
pub fn build_return_to_agent_wakeup(input: &ReturnToAgentInput) -> ReturnToAgentWakeup {
    let mut structured = serde_json::json!({
        "event": "workbench_reconciled",
        "workbench_id": input.workbench_id,
        "base_artifact_id": input.base_artifact_id,
        "reconciled": input.reconciled,
        "unsaved_change_count": input.unsaved_change_count,
        "operator_modified": !input.operator_modified_files.is_empty()
            || !input.operator_added_files.is_empty()
            || !input.deleted_files.is_empty(),
    });

    if let Some(new_artifact_ref) = &input.new_artifact_ref {
        structured["new_artifact_ref"] = serde_json::Value::String(new_artifact_ref.clone());
    }
    if let Some(new_artifact_id) = &input.new_artifact_id {
        structured["new_artifact_id"] = serde_json::Value::String(new_artifact_id.clone());
    }
    if !input.operator_modified_files.is_empty() {
        structured["operator_modified_files"] = serde_json::Value::Array(
            input
                .operator_modified_files
                .iter()
                .map(|s| serde_json::Value::String(s.clone()))
                .collect(),
        );
    }
    if !input.operator_added_files.is_empty() {
        structured["operator_added_files"] = serde_json::Value::Array(
            input
                .operator_added_files
                .iter()
                .map(|s| serde_json::Value::String(s.clone()))
                .collect(),
        );
    }
    if !input.deleted_files.is_empty() {
        structured["deleted_files"] = serde_json::Value::Array(
            input
                .deleted_files
                .iter()
                .map(|s| serde_json::Value::String(s.clone()))
                .collect(),
        );
    }
    if let Some(semantic_summary) = &input.semantic_summary {
        structured["semantic_summary"] = serde_json::to_value(semantic_summary)
            .unwrap_or(serde_json::Value::Null);
    }

    let artifact_ref_label = input
        .new_artifact_ref
        .as_ref()
        .map(|r| format!("`{}`", r))
        .unwrap_or_else(|| format!("base `{}`", input.base_artifact_id));

    let mut message = String::new();
    message.push_str(&format!(
        "Operator returned workbench `{}` to you. Active artifact: {}.",
        input.workbench_id, artifact_ref_label
    ));
    if input.reconciled {
        message.push_str(" Status: reconciled.");
    } else if input.unsaved_change_count > 0 {
        message.push_str(&format!(
            " Status: active with {} unsaved change(s) (sent with --force; edits were not committed).",
            input.unsaved_change_count
        ));
    } else {
        message.push_str(" Status: active, in sync with base artifact (no edits).");
    }
    if let Some(note) = &input.operator_note {
        if !note.trim().is_empty() {
            message.push_str(&format!(" Operator note: {}.", note.trim()));
        }
    }
    if let Some(semantic_summary) = &input.semantic_summary {
        if let Some(label) = summarize_contract_changes(semantic_summary) {
            message.push_str(&format!(" Contract impact: {label}."));
        }
    }
    message.push_str(" Please continue the workflow.");

    ReturnToAgentWakeup {
        message,
        metadata: serde_json::json!({
            "workbench_reconciled": structured,
        }),
    }
}

/// Render a one-line summary of the contract-impact changes recorded in
/// the semantic summary. Returns `None` when there are no
/// `contract_changes` to call out.
///
/// Format: a comma-separated list of `"<impact> on <path>"` items,
/// truncated to the first three to keep the wake-up message compact.
pub fn summarize_contract_changes(summary: &SemanticSummary) -> Option<String> {
    if summary.contract_changes.is_empty() {
        return None;
    }
    let mut labels: Vec<String> = Vec::new();
    for c in summary.contract_changes.iter().take(3) {
        labels.push(format!("{} on {}", c.impact.as_str(), c.path));
    }
    let suffix = if summary.contract_changes.len() > 3 {
        format!(" (+{} more)", summary.contract_changes.len() - 3)
    } else {
        String::new()
    };
    Some(format!("{}{suffix}", labels.join(", ")))
}

/// Read the workbench's operator-edited file lists from the gateway store.
/// Returns `None` when the workbench is not found or the workspace dir is gone.
///
/// For a *reconciled* workbench, the data is sourced from
/// `.autonoetic/reconciliation.json` (written by the workbench_reconcile
/// tool) so the wake-up carries the new artifact ref/id and the
/// operator-vs-agent authorship classification recorded at reconcile time.
/// For an *active* workbench, the data is computed live from
/// `base_digests.json` and the current files on disk.
pub fn read_return_to_agent_input(
    gateway_store: &GatewayStore,
    workbench_id: &str,
    operator_note: Option<&str>,
) -> Option<ReturnToAgentInput> {
    let wb = gateway_store.load_workbench(workbench_id).ok().flatten()?;

    let reconciled = matches!(wb.status, WorkbenchStatus::Reconciled);

    let source_dir = Path::new(&wb.workspace_path);
    let meta_dir = source_dir.parent().map(|p| p.join(".autonoetic"));

    if reconciled {
        let provenance_path = meta_dir.as_ref().map(|d| d.join("reconciliation.json"));
        let provenance: Option<serde_json::Value> = provenance_path
            .as_ref()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|raw| serde_json::from_str(&raw).ok());

        let new_artifact_ref = provenance
            .as_ref()
            .and_then(|v| v.get("new_artifact_ref"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let new_artifact_id = provenance
            .as_ref()
            .and_then(|v| v.get("new_artifact_id"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let modified: Vec<String> = provenance
            .as_ref()
            .and_then(|v| v.get("operator_modified"))
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|s| s.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default();
        let added: Vec<String> = provenance
            .as_ref()
            .and_then(|v| v.get("operator_added"))
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|s| s.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default();
        let deleted: Vec<String> = provenance
            .as_ref()
            .and_then(|v| v.get("deleted"))
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|s| s.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default();

        let unsaved_change_count = modified.len() + added.len() + deleted.len();
        let semantic_summary = meta_dir
            .as_ref()
            .map(|d| d.join("semantic_summary.json"))
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|raw| serde_json::from_str(&raw).ok());
        return Some(ReturnToAgentInput {
            workbench_id: wb.workbench_id,
            base_artifact_id: wb.base_artifact_id,
            reconciled,
            new_artifact_ref,
            new_artifact_id,
            operator_note: operator_note.map(|s| s.to_string()),
            unsaved_change_count,
            operator_modified_files: modified,
            operator_added_files: added,
            deleted_files: deleted,
            semantic_summary,
        });
    }

    // Active workbench: compute live from base_digests + current files.
    if !source_dir.exists() {
        return Some(ReturnToAgentInput {
            workbench_id: wb.workbench_id,
            base_artifact_id: wb.base_artifact_id,
            reconciled,
            new_artifact_ref: None,
            new_artifact_id: None,
            operator_note: operator_note.map(|s| s.to_string()),
            unsaved_change_count: 0,
            operator_modified_files: Vec::new(),
            operator_added_files: Vec::new(),
            deleted_files: Vec::new(),
            semantic_summary: None,
        });
    }

    let base_digests: HashMap<String, String> = meta_dir
        .as_ref()
        .and_then(|d| {
            let p = d.join("base_digests.json");
            if p.exists() {
                serde_json::from_str(&std::fs::read_to_string(p).ok()?).ok()
            } else {
                None
            }
        })
        .unwrap_or_default();

    let mut current_names: Vec<String> = Vec::new();
    for entry in walkdir::WalkDir::new(source_dir)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        if entry.file_type().is_file() {
            let rel = entry.path().strip_prefix(source_dir).unwrap();
            let rel_str = rel.to_string_lossy().to_string();
            if rel_str.starts_with(".autonoetic/") {
                continue;
            }
            current_names.push(rel_str);
        }
    }
    let current_set: std::collections::HashSet<&str> =
        current_names.iter().map(|s| s.as_str()).collect();

    let mut modified: Vec<String> = Vec::new();
    let mut added: Vec<String> = Vec::new();
    let mut deleted: Vec<String> = Vec::new();
    for name in &current_names {
        match base_digests.get(name.as_str()) {
            Some(base_digest) => {
                match file_sha256(&source_dir.join(name)) {
                    Ok(current_digest) if current_digest == *base_digest => {}
                    _ => modified.push(name.clone()),
                }
            }
            None => added.push(name.clone()),
        }
    }
    for name in base_digests.keys() {
        if !current_set.contains(name.as_str()) {
            deleted.push(name.clone());
        }
    }

    let unsaved_change_count = modified.len() + added.len() + deleted.len();

    Some(ReturnToAgentInput {
        workbench_id: wb.workbench_id,
        base_artifact_id: wb.base_artifact_id,
        reconciled,
        new_artifact_ref: None,
        new_artifact_id: None,
        operator_note: operator_note.map(|s| s.to_string()),
        unsaved_change_count,
        operator_modified_files: modified,
        operator_added_files: added,
        deleted_files: deleted,
        semantic_summary: None,
    })
}

/// Outcome of `prepare_return_to_agent_wakeup`. Drives the TUI's response
/// to a `/return` slash command: either render an inline error and stop, or
/// dispatch the wake-up to the orchestrator.
#[derive(Debug)]
pub enum ReturnToAgentStatus {
    /// Workbench has unsaved edits and `--force` was not supplied, or the
    /// workbench is no longer in the store. TUI shows the refusal and stops.
    Refused { reason: String },
    /// Wake-up is built and ready to send. TUI dispatches via the channel.
    Ready {
        target_agent_id: String,
        outbound_message: String,
        metadata: serde_json::Value,
    },
}

/// Prepare the wake-up that `/return` will dispatch. Pulls the workbench
/// from the gateway store, applies the unsaved-edits safety check, and
/// produces the structured payload for `event.ingest`.
pub fn prepare_return_to_agent_wakeup(
    gateway_store: &GatewayStore,
    workbench_id: &str,
    force: bool,
    operator_note: Option<&str>,
) -> ReturnToAgentStatus {
    let Some(input) = read_return_to_agent_input(gateway_store, workbench_id, operator_note) else {
        return ReturnToAgentStatus::Refused {
            reason: format!(
                "Workbench {workbench_id} is no longer in the gateway store. Cannot return."
            ),
        };
    };

    if !force && !input.reconciled && input.unsaved_change_count > 0 {
        let mut lines = Vec::new();
        lines.push(format!(
            "Workbench {} has {} unsaved edit(s). Refusing to silently drop them.",
            input.workbench_id, input.unsaved_change_count
        ));
        if !input.operator_modified_files.is_empty() {
            lines.push("  Modified:".to_string());
            for f in &input.operator_modified_files {
                lines.push(format!("    ~ {}", f));
            }
        }
        if !input.operator_added_files.is_empty() {
            lines.push("  Added:".to_string());
            for f in &input.operator_added_files {
                lines.push(format!("    + {}", f));
            }
        }
        if !input.deleted_files.is_empty() {
            lines.push("  Deleted:".to_string());
            for f in &input.deleted_files {
                lines.push(format!("    - {}", f));
            }
        }
        lines.push(
            "Reconcile them first (autonoetic workbench reconcile <wb>) or re-run with --force to drop the edits and return the base artifact.".to_string(),
        );
        return ReturnToAgentStatus::Refused {
            reason: lines.join("\n"),
        };
    }

    let target_agent_id = "planner.default".to_string();
    let wakeup = build_return_to_agent_wakeup(&input);

    ReturnToAgentStatus::Ready {
        target_agent_id,
        outbound_message: wakeup.message,
        metadata: wakeup.metadata,
    }
}

fn file_sha256(path: &Path) -> anyhow::Result<String> {
    let data = std::fs::read(path)?;
    let mut hasher = sha2::Sha256::new();
    hasher.update(&data);
    Ok(format!("sha256:{}", hex::encode(hasher.finalize())))
}

#[cfg(test)]
mod tests {
    use super::*;
    use autonoetic_types::semantic_diff::{ContractChange, ContractImpact};
    use autonoetic_types::workbench::FileChangeType;

    #[test]
    fn build_wakeup_reconciled_with_note_and_summary() {
        let input = ReturnToAgentInput {
            workbench_id: "wb-test".into(),
            base_artifact_id: "ar.base".into(),
            reconciled: true,
            new_artifact_ref: Some("ar.new".into()),
            new_artifact_id: Some("aid".into()),
            operator_note: Some("looks good".into()),
            unsaved_change_count: 0,
            operator_modified_files: vec!["a.txt".into()],
            operator_added_files: vec![],
            deleted_files: vec![],
            semantic_summary: Some(SemanticSummary {
                workbench_id: "wb-test".into(),
                base_artifact_id: "ar.base".into(),
                new_artifact_id: "aid".into(),
                plan_id: None,
                plan_version: None,
                total_files: 1,
                changed_files: 1,
                added_files: 0,
                modified_files: 1,
                deleted_files: 0,
                contract_changes: vec![ContractChange {
                    path: "a.txt".into(),
                    change_type: FileChangeType::Modified,
                    impact: ContractImpact::CapabilityChange,
                    rationale: "matches capabilities.yaml".into(),
                }],
                file_classifications: vec![],
                validation_state: Default::default(),
                summarizer_id: "rule_based_v1".into(),
                generated_at: "2026-06-01T00:00:00Z".into(),
            }),
        };
        let wakeup = build_return_to_agent_wakeup(&input);
        assert!(wakeup.message.contains("Operator returned workbench `wb-test`"));
        assert!(wakeup.message.contains("Status: reconciled"));
        assert!(wakeup.message.contains("Operator note: looks good"));
        assert!(wakeup.message.contains("Contract impact: capability_change on a.txt"));
        let meta = wakeup.metadata.get("workbench_reconciled").unwrap();
        assert_eq!(meta.get("reconciled").unwrap().as_bool(), Some(true));
        assert_eq!(meta.get("new_artifact_ref").unwrap().as_str(), Some("ar.new"));
    }

    #[test]
    fn summarize_contract_changes_truncates_and_suffixes() {
        let summary = SemanticSummary {
            workbench_id: "wb".into(),
            base_artifact_id: "ar.base".into(),
            new_artifact_id: "ar.new".into(),
            plan_id: None,
            plan_version: None,
            total_files: 4,
            changed_files: 4,
            added_files: 0,
            modified_files: 4,
            deleted_files: 0,
            contract_changes: vec![
                ContractChange {
                    path: "a.txt".into(),
                    change_type: FileChangeType::Modified,
                    impact: ContractImpact::CapabilityChange,
                    rationale: "".into(),
                },
                ContractChange {
                    path: "b.txt".into(),
                    change_type: FileChangeType::Modified,
                    impact: ContractImpact::SkillManifestChange,
                    rationale: "".into(),
                },
                ContractChange {
                    path: "c.txt".into(),
                    change_type: FileChangeType::Modified,
                    impact: ContractImpact::RuntimeLockChange,
                    rationale: "".into(),
                },
                ContractChange {
                    path: "d.txt".into(),
                    change_type: FileChangeType::Modified,
                    impact: ContractImpact::NetworkAccessChange,
                    rationale: "".into(),
                },
            ],
            file_classifications: vec![],
            validation_state: Default::default(),
            summarizer_id: "rule_based_v1".into(),
            generated_at: "2026-06-01T00:00:00Z".into(),
        };
        let s = summarize_contract_changes(&summary).unwrap();
        assert!(s.contains("a.txt"));
        assert!(s.contains("b.txt"));
        assert!(s.contains("c.txt"));
        assert!(!s.contains("d.txt"));
        assert!(s.contains("(+1 more)"));
    }
}
