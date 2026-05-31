use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PlanRef {
    pub plan_id: String,
    pub version: u32,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PlanStatus {
    Draft,
    AwaitingApproval,
    Approved,
    Superseded,
    Completed,
    Cancelled,
}

impl PlanStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            PlanStatus::Draft => "draft",
            PlanStatus::AwaitingApproval => "awaiting_approval",
            PlanStatus::Approved => "approved",
            PlanStatus::Superseded => "superseded",
            PlanStatus::Completed => "completed",
            PlanStatus::Cancelled => "cancelled",
        }
    }
}

impl Default for PlanStatus {
    fn default() -> Self {
        Self::Draft
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StepOwner {
    Planner,
    Agent,
    Operator,
    Shared,
}

impl StepOwner {
    pub fn as_str(&self) -> &'static str {
        match self {
            StepOwner::Planner => "planner",
            StepOwner::Agent => "agent",
            StepOwner::Operator => "operator",
            StepOwner::Shared => "shared",
        }
    }
}

impl Default for StepOwner {
    fn default() -> Self {
        Self::Planner
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StepStatus {
    Pending,
    InProgress,
    Completed,
    Skipped,
    Blocked,
}

impl StepStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            StepStatus::Pending => "pending",
            StepStatus::InProgress => "in_progress",
            StepStatus::Completed => "completed",
            StepStatus::Skipped => "skipped",
            StepStatus::Blocked => "blocked",
        }
    }
}

impl Default for StepStatus {
    fn default() -> Self {
        Self::Pending
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PlanStep {
    pub step_id: String,
    pub title: String,
    #[serde(default)]
    pub owner: StepOwner,
    #[serde(default)]
    pub status: StepStatus,
    #[serde(default)]
    pub depends_on: Vec<String>,
    #[serde(default)]
    pub task_ids: Vec<String>,
    #[serde(default)]
    pub artifact_refs: Vec<String>,
    #[serde(default)]
    pub agent_id: Option<String>,
    #[serde(default)]
    pub notes: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ValidationClass {
    MechanicalSafety,
    SecurityReview,
    CorrectnessCheck,
    QualityCheck,
    PackagingCheck,
}

impl ValidationClass {
    pub fn as_str(&self) -> &'static str {
        match self {
            ValidationClass::MechanicalSafety => "mechanical_safety",
            ValidationClass::SecurityReview => "security_review",
            ValidationClass::CorrectnessCheck => "correctness_check",
            ValidationClass::QualityCheck => "quality_check",
            ValidationClass::PackagingCheck => "packaging_check",
        }
    }
}

impl Default for ValidationClass {
    fn default() -> Self {
        Self::CorrectnessCheck
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ValidationRequirement {
    Required,
    Advisory,
    Waived,
}

impl ValidationRequirement {
    pub fn as_str(&self) -> &'static str {
        match self {
            ValidationRequirement::Required => "required",
            ValidationRequirement::Advisory => "advisory",
            ValidationRequirement::Waived => "waived",
        }
    }
}

impl Default for ValidationRequirement {
    fn default() -> Self {
        Self::Advisory
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ValidationEntry {
    pub validation_id: String,
    pub title: String,
    #[serde(default)]
    pub class: ValidationClass,
    #[serde(default)]
    pub requirement: ValidationRequirement,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ValidationPolicy {
    #[serde(default)]
    pub entries: Vec<ValidationEntry>,
}

impl Default for ValidationPolicy {
    fn default() -> Self {
        Self {
            entries: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PlanFrame {
    pub plan_id: String,
    pub workflow_id: String,
    pub root_session_id: String,
    pub title: String,
    pub objective: String,
    #[serde(default)]
    pub status: PlanStatus,
    pub version: u32,
    #[serde(default)]
    pub steps: Vec<PlanStep>,
    #[serde(default)]
    pub validation_policy: ValidationPolicy,
    #[serde(default)]
    pub approved_by: Option<String>,
    #[serde(default)]
    pub approved_at: Option<String>,
    pub created_by_agent_id: String,
    pub updated_at: String,
    pub created_at: String,
}

impl PlanFrame {
    pub fn compact_summary(&self) -> PlanFrameSummary {
        let operator_steps: Vec<String> = self
            .steps
            .iter()
            .filter(|s| s.owner == StepOwner::Operator || s.owner == StepOwner::Shared)
            .map(|s| s.step_id.clone())
            .collect();

        let current_steps: Vec<String> = self
            .steps
            .iter()
            .filter(|s| s.status == StepStatus::InProgress)
            .map(|s| s.step_id.clone())
            .collect();

        let required_validations: Vec<String> = self
            .validation_policy
            .entries
            .iter()
            .filter(|v| v.requirement == ValidationRequirement::Required)
            .map(|v| v.validation_id.clone())
            .collect();

        let advisory_validations: Vec<String> = self
            .validation_policy
            .entries
            .iter()
            .filter(|v| v.requirement == ValidationRequirement::Advisory)
            .map(|v| v.validation_id.clone())
            .collect();

        PlanFrameSummary {
            plan_id: self.plan_id.clone(),
            version: self.version,
            status: self.status,
            title: self.title.clone(),
            current_steps,
            operator_steps,
            required_validations,
            advisory_validations,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PlanFrameSummary {
    pub plan_id: String,
    pub version: u32,
    pub status: PlanStatus,
    pub title: String,
    pub current_steps: Vec<String>,
    pub operator_steps: Vec<String>,
    pub required_validations: Vec<String>,
    pub advisory_validations: Vec<String>,
}
