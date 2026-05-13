use serde::{Deserialize, Serialize};

/// Tracks a single recording run — the operator's `--record-network` session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecordingSession {
    pub session_id: String,
    pub agent_id: String,
    pub artifact_id: String,
    pub revision_id: String,
    pub root_session_id: String,
    pub started_at: String,
    pub stopped_at: Option<String>,
    pub duration_secs: Option<i64>,
    pub max_requests: Option<i64>,
    pub max_bytes: Option<i64>,
    pub request_count: i64,
    pub total_bytes: i64,
    pub status: RecordingStatus,
    pub fixture_set_id: Option<String>,
    pub created_by: String,
}

/// Status of a recording session.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RecordingStatus {
    #[default]
    Active,
    Completed,
    Cancelled,
    Failed,
}

impl RecordingStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            RecordingStatus::Active => "active",
            RecordingStatus::Completed => "completed",
            RecordingStatus::Cancelled => "cancelled",
            RecordingStatus::Failed => "failed",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "active" => Some(RecordingStatus::Active),
            "completed" => Some(RecordingStatus::Completed),
            "cancelled" => Some(RecordingStatus::Cancelled),
            "failed" => Some(RecordingStatus::Failed),
            _ => None,
        }
    }
}

/// A content-addressed set of recorded HTTP fixtures.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FixtureSet {
    pub fixture_set_id: String,
    pub agent_id: String,
    pub revision_id: String,
    pub recording_session_id: String,
    pub created_at: String,
    pub fixture_file_count: i64,
    pub total_bytes: i64,
    pub digest: String,
    pub host_summary: Vec<String>,
    pub host_count: i64,
    pub redaction_summary: Vec<String>,
    pub status: FixtureSetStatus,
}

/// Status of a fixture set.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FixtureSetStatus {
    #[default]
    Ready,
    Expired,
    Invalid,
}

impl FixtureSetStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            FixtureSetStatus::Ready => "ready",
            FixtureSetStatus::Expired => "expired",
            FixtureSetStatus::Invalid => "invalid",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "ready" => Some(FixtureSetStatus::Ready),
            "expired" => Some(FixtureSetStatus::Expired),
            "invalid" => Some(FixtureSetStatus::Invalid),
            _ => None,
        }
    }
}
