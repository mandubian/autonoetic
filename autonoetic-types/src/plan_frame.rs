use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PlanRef {
    pub plan_id: String,
    pub version: u32,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PlanStatus {
    AwaitingApproval,
    Approved,
    Completed,
    Cancelled,
}

impl PlanStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            PlanStatus::AwaitingApproval => "awaiting_approval",
            PlanStatus::Approved => "approved",
            PlanStatus::Completed => "completed",
            PlanStatus::Cancelled => "cancelled",
        }
    }
}

impl Default for PlanStatus {
    fn default() -> Self {
        Self::AwaitingApproval
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PlanStep {
    pub step_id: String,
    pub title: String,
    #[serde(default)]
    pub owner: StepOwner,
    #[serde(default)]
    pub depends_on: Vec<String>,
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
    pub version: u32,
    pub parent_version: Option<u32>,
    pub workflow_id: String,
    pub root_session_id: String,
    pub title: String,
    pub objective: String,
    #[serde(default)]
    pub status: PlanStatus,
    #[serde(default)]
    pub steps: Vec<PlanStep>,
    #[serde(default)]
    pub validation_policy: ValidationPolicy,
    #[serde(default)]
    pub approved_by: Option<String>,
    #[serde(default)]
    pub approved_at: Option<String>,
    pub created_by_agent_id: String,
    pub reason: Option<String>,
    pub created_at: String,
}

/// Suggest a foundational `agent_id` from step title when the plan omits one.
pub fn infer_agent_id_from_step_title(title: &str) -> Option<String> {
    let t = title.to_lowercase();
    if t.contains("research")
        || t.contains("data source")
        || t.contains("market data")
        || (t.contains("api") && (t.contains("source") || t.contains("survey")))
    {
        return Some("researcher.default".to_string());
    }
    if t.contains("architect")
        || t.contains("architecture")
        || t.contains("design")
        || t.contains("data flow")
    {
        return Some("architect.default".to_string());
    }
    if t.contains("implement")
        || t.contains("coding")
        || t.contains("build")
        || t.contains("develop")
    {
        return Some("coder.default".to_string());
    }
    if t.contains("federation")
        || t.contains("audit")
        || t.contains("evaluat")
        || t.contains("promotion")
    {
        return Some("auditor.default".to_string());
    }
    None
}

impl PlanStep {
    /// Explicit `agent_id` on the step, or a title-based foundational default.
    pub fn resolved_agent_id(&self) -> Option<String> {
        if let Some(id) = &self.agent_id {
            let trimmed = id.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
        if self.owner == StepOwner::Operator {
            return None;
        }
        infer_agent_id_from_step_title(&self.title)
    }
}

impl PlanFrame {
    /// First agent-owned step in plan order (skips operator-only steps).
    pub fn first_agent_step(&self) -> Option<&PlanStep> {
        self.steps
            .iter()
            .find(|s| s.owner == StepOwner::Agent || s.owner == StepOwner::Shared)
    }

    /// Operator-facing hint after approval: what to spawn next without `agent_list`.
    pub fn execution_wake_hint(&self) -> Option<String> {
        if self.status != PlanStatus::Approved {
            return None;
        }
        let step = self.first_agent_step()?;
        let agent_id = step.resolved_agent_id()?;
        Some(format!(
            "Start approved plan {} v{} at step {} ({}): call agent_spawn on `{}` with a task message drawn from the plan objective. Do not call agent_list or agent_discover — agent_id is already known.",
            self.plan_id,
            self.version,
            step.step_id,
            step.title,
            agent_id
        ))
    }

    pub fn compact_summary(&self) -> PlanFrameSummary {
        let operator_steps: Vec<String> = self
            .steps
            .iter()
            .filter(|s| s.owner == StepOwner::Operator || s.owner == StepOwner::Shared)
            .map(|s| s.step_id.clone())
            .collect();

        let agent_steps: Vec<String> = self
            .steps
            .iter()
            .filter(|s| s.owner == StepOwner::Agent)
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
            parent_version: self.parent_version,
            status: self.status,
            title: self.title.clone(),
            step_count: self.steps.len(),
            operator_steps,
            agent_steps,
            required_validations,
            advisory_validations,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PlanFrameSummary {
    pub plan_id: String,
    pub version: u32,
    pub parent_version: Option<u32>,
    pub status: PlanStatus,
    pub title: String,
    pub step_count: usize,
    pub operator_steps: Vec<String>,
    pub agent_steps: Vec<String>,
    pub required_validations: Vec<String>,
    pub advisory_validations: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ValidationWaiver {
    pub waiver_id: String,
    pub workflow_id: String,
    pub artifact_id: String,
    pub validation_id: String,
    pub validation_class: ValidationClass,
    pub waived_by: String,
    pub reason: String,
    pub created_at: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn infer_agent_from_research_step_title() {
        assert_eq!(
            infer_agent_id_from_step_title("Research market data sources and APIs").as_deref(),
            Some("researcher.default")
        );
    }

    #[test]
    fn execution_wake_hint_requires_approved_status() {
        let mut plan = PlanFrame {
            plan_id: "plan-x".into(),
            version: 1,
            parent_version: None,
            workflow_id: "wf".into(),
            root_session_id: "s".into(),
            title: "T".into(),
            objective: "O".into(),
            status: PlanStatus::AwaitingApproval,
            steps: vec![PlanStep {
                step_id: "s1".into(),
                title: "Research APIs".into(),
                owner: StepOwner::Agent,
                depends_on: vec![],
                agent_id: None,
                notes: None,
            }],
            validation_policy: ValidationPolicy::default(),
            approved_by: None,
            approved_at: None,
            created_by_agent_id: "planner".into(),
            reason: None,
            created_at: "now".into(),
        };
        assert!(plan.execution_wake_hint().is_none());
        plan.status = PlanStatus::Approved;
        let hint = plan.execution_wake_hint().expect("hint");
        assert!(hint.contains("researcher.default"));
        assert!(hint.contains("agent_spawn"));
        assert!(hint.contains("agent_list"));
    }
}
