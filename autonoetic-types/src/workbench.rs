use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WorkbenchStatus {
    Active,
    Reconciled,
    Discarded,
}

impl WorkbenchStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            WorkbenchStatus::Active => "active",
            WorkbenchStatus::Reconciled => "reconciled",
            WorkbenchStatus::Discarded => "discarded",
        }
    }
}

impl Default for WorkbenchStatus {
    fn default() -> Self {
        Self::Active
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WorkbenchProjection {
    pub workbench_id: String,
    pub workflow_id: String,
    pub root_session_id: String,
    pub plan_id: Option<String>,
    pub base_artifact_id: String,
    pub base_artifact_canonical_digest: String,
    pub workspace_path: String,
    pub status: WorkbenchStatus,
    pub created_by_agent_id: String,
    pub created_at: String,
    pub last_checkpoint_at: Option<String>,
    pub reconciled_at: Option<String>,
    pub discarded_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkbenchCheckpoint {
    pub checkpoint_id: String,
    pub workbench_id: String,
    pub label: Option<String>,
    pub file_count: usize,
    pub total_bytes: u64,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WorkbenchFileDiff {
    pub path: String,
    pub change_type: FileChangeType,
    pub base_digest: Option<String>,
    pub current_digest: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FileChangeType {
    Unchanged,
    Added,
    Modified,
    Deleted,
}

impl FileChangeType {
    pub fn as_str(&self) -> &'static str {
        match self {
            FileChangeType::Unchanged => "unchanged",
            FileChangeType::Added => "added",
            FileChangeType::Modified => "modified",
            FileChangeType::Deleted => "deleted",
        }
    }
}
