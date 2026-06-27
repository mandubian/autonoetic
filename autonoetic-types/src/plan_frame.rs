use serde::{Deserialize, Serialize};

use crate::capability::{compute_capability_delta, Capability, CapabilityDelta};

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

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StepStatus {
    Pending,
    InProgress,
    Completed,
    Failed,
    Skipped,
}

impl StepStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            StepStatus::Pending => "pending",
            StepStatus::InProgress => "in_progress",
            StepStatus::Completed => "completed",
            StepStatus::Failed => "failed",
            StepStatus::Skipped => "skipped",
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
    pub depends_on: Vec<String>,
    #[serde(default)]
    pub agent_id: Option<String>,
    #[serde(default)]
    pub notes: Option<String>,
    #[serde(default)]
    pub status: StepStatus,
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
    /// Optional capability envelope for this plan. If present and non-empty,
    /// plan approval proposes locking this envelope for the session.
    /// If empty, the session-level discovery mechanism drives.
    #[serde(default)]
    pub capability_envelope: Vec<Capability>,
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

/// Validate that `depends_on` entries form a well-formed DAG:
/// - All referenced `step_id`s exist
/// - No self-dependencies
/// - No cycles
///
/// Returns `Ok(())` if valid, or `Err(message)` with a human-readable description
/// of the first problem found.
pub fn validate_step_dag(steps: &[PlanStep]) -> Result<(), String> {
    let ids: std::collections::HashSet<&str> =
        steps.iter().map(|s| s.step_id.as_str()).collect();

    // Check for duplicate step_ids.
    if ids.len() != steps.len() {
        let mut seen = std::collections::HashSet::new();
        for s in steps {
            if !seen.insert(s.step_id.as_str()) {
                return Err(format!(
                    "duplicate step_id `{}` — step_ids must be unique within a plan",
                    s.step_id
                ));
            }
        }
    }

    // Check references and self-deps.
    for s in steps {
        for dep in &s.depends_on {
            if dep == &s.step_id {
                return Err(format!(
                    "step `{}` depends on itself",
                    s.step_id
                ));
            }
            if !ids.contains(dep.as_str()) {
                return Err(format!(
                    "step `{}` depends on `{}` which does not exist in this plan",
                    s.step_id, dep
                ));
            }
        }
    }

    // Cycle detection via DFS.
    let mut visited = std::collections::HashMap::new();
    for step in steps {
        if dfs_has_cycle(step.step_id.as_str(), steps, &mut visited) {
            return Err(format!(
                "cycle detected in plan step dependencies involving `{}`",
                step.step_id
            ));
        }
    }

    Ok(())
}

fn dfs_has_cycle(
    start: &str,
    steps: &[PlanStep],
    visited: &mut std::collections::HashMap<String, u8>,
) -> bool {
    // visited: 0 = unvisited, 1 = in progress (on stack), 2 = done
    match visited.get(start) {
        Some(&2) => return false,
        Some(&1) => return true,
        _ => {}
    }
    visited.insert(start.to_string(), 1);

    let step = steps.iter().find(|s| s.step_id == start);
    if let Some(s) = step {
        for dep in &s.depends_on {
            if dfs_has_cycle(dep, steps, visited) {
                return true;
            }
        }
    }

    visited.insert(start.to_string(), 2);
    false
}

/// Find steps that `step_id` depends on but which are not yet `Completed`.
/// Returns a list of `(dep_step_id, status)` tuples for unsatisfied deps.
pub fn unsatisfied_dependencies(
    plan: &PlanFrame,
    step_id: &str,
) -> Vec<(String, StepStatus)> {
    let step = plan.steps.iter().find(|s| s.step_id == step_id);
    let Some(step) = step else {
        return Vec::new();
    };

    step.depends_on
        .iter()
        .filter_map(|dep_id| {
            plan.steps.iter().find(|s| &s.step_id == dep_id).map(|dep| {
                if dep.status != StepStatus::Completed {
                    Some((dep.step_id.clone(), dep.status))
                } else {
                    None
                }
            }).flatten()
        })
        .collect()
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

/// Params for `planframes.list_pending` (Session Room / operator clients).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanFramesListPendingParams {
    pub root_session_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanFramesListPendingResult {
    pub plans: Vec<PlanFrame>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanFramesApproveParams {
    pub plan_id: String,
    #[serde(default = "default_plan_approver")]
    pub approved_by: String,
}

fn default_plan_approver() -> String {
    "operator".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanFramesApproveResult {
    pub plan: PlanFrame,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanFramesGetParams {
    pub plan_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanFramesGetResult {
    pub plan: PlanFrame,
}

/// Structural diff of the safety-relevant **envelope** between two plan
/// revisions. Used by `planframe_amend` to decide whether an amendment
/// **inherits** the prior operator approval (no envelope change) or
/// **re-opens the gate** (envelope expansion). This is a mechanical,
/// gateway-computed classification — never delegated to the LLM.
///
/// The envelope is what could expand the capability/safety surface of the
/// work: which steps exist, who owns them, which agent runs them, and the
/// validation gates that bound the work. Rewording the objective/title or
/// recording a progress `reason` is NOT an envelope change and inherits.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanEnvelopeDiff {
    /// `step_id`s present in the new revision but not the parent. New work ⇒
    /// re-gate (the operator didn't consent to it).
    pub steps_added: Vec<String>,
    /// `step_id`s removed. Removing work changes the shape the operator
    /// approved ⇒ re-gate.
    pub steps_removed: Vec<String>,
    /// `(step_id, old_owner, new_owner)` — e.g. a step moving to `Operator`
    /// or from `Agent` to `Shared`. Changes accountability ⇒ re-gate.
    pub owners_changed: Vec<(String, StepOwner, StepOwner)>,
    /// `(step_id, old_agent_id, new_agent_id)` — a different agent means a
    /// different capability set ⇒ re-gate.
    pub agents_changed: Vec<(String, Option<String>, Option<String>)>,
    /// `validation_id`s removed. Less validation ⇒ riskier ⇒ re-gate.
    pub validation_removed: Vec<String>,
    /// `(validation_id, old_requirement, new_requirement)` where the new
    /// requirement is weaker (Required → Advisory/Waived). Weakening a gate
    /// ⇒ re-gate. Strengthening does NOT (more safety is fine).
    pub validation_weakened: Vec<(String, ValidationRequirement, ValidationRequirement)>,
    /// Broadening/narrowing of the declared `capability_envelope` between revisions.
    pub capability_delta: CapabilityDelta,
}

impl PlanEnvelopeDiff {
    /// True when the amendment expands the safety/capability envelope and the
    /// operator must re-approve. Conservative: any envelope touch ⇒ re-gate.
    pub fn requires_regate(&self) -> bool {
        !self.steps_added.is_empty()
            || !self.steps_removed.is_empty()
            || !self.owners_changed.is_empty()
            || !self.agents_changed.is_empty()
            || !self.validation_removed.is_empty()
            || !self.validation_weakened.is_empty()
            || self.capability_delta.has_broadening()
    }

    /// True when the only changes are cosmetic/progress (objective rewording,
    /// title tweak, reason note). The approval is inherited in this case.
    pub fn is_cosmetic_only(&self) -> bool {
        !self.requires_regate()
    }

    /// Compact one-line human summary, e.g.
    /// `+step s5  owner s3→operator  −validation v2`. Returns the literal
    /// `"no envelope change"` for a cosmetic-only diff (callers display this
    /// to explain why the operator was not asked to re-approve).
    pub fn summary(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        if !self.steps_added.is_empty() {
            parts.push(format!("+step {}", self.steps_added.join(",")));
        }
        if !self.steps_removed.is_empty() {
            parts.push(format!("−step {}", self.steps_removed.join(",")));
        }
        for (sid, old, new) in &self.owners_changed {
            parts.push(format!("owner {sid}:{}→{}", old.as_str(), new.as_str()));
        }
        for (sid, old, new) in &self.agents_changed {
            parts.push(format!(
                "agent {sid}:{}→{}",
                old.as_deref().unwrap_or("—"),
                new.as_deref().unwrap_or("—")
            ));
        }
        if !self.validation_removed.is_empty() {
            parts.push(format!("−validation {}", self.validation_removed.join(",")));
        }
        for (vid, old, new) in &self.validation_weakened {
            parts.push(format!("weaken {vid}:{}→{}", old.as_str(), new.as_str()));
        }
        if self.capability_delta.has_broadening() {
            let mut cap_parts: Vec<String> = self.capability_delta.added.clone();
            for b in &self.capability_delta.broadened {
                cap_parts.push(b.capability_type.clone());
            }
            cap_parts.sort();
            parts.push(format!("+capability {}", cap_parts.join(",")));
        }
        if parts.is_empty() {
            "no envelope change".to_string()
        } else {
            parts.join("  ")
        }
    }
}

/// Compute the envelope diff between a parent revision and a proposed child.
/// Both are immutable `PlanFrame`s, so this is a pure function. Step identity
/// is by `step_id`; validation identity by `validation_id`.
pub fn plan_envelope_diff(parent: &PlanFrame, child: &PlanFrame) -> PlanEnvelopeDiff {
    let parent_steps: std::collections::HashMap<&str, &PlanStep> = parent
        .steps
        .iter()
        .map(|s| (s.step_id.as_str(), s))
        .collect();
    let child_steps: std::collections::HashMap<&str, &PlanStep> = child
        .steps
        .iter()
        .map(|s| (s.step_id.as_str(), s))
        .collect();

    let mut steps_added = Vec::new();
    let mut steps_removed = Vec::new();
    let mut owners_changed = Vec::new();
    let mut agents_changed = Vec::new();

    for sid in child_steps.keys() {
        if !parent_steps.contains_key(sid) {
            steps_added.push(sid.to_string());
        }
    }
    for (sid, pstep) in &parent_steps {
        match child_steps.get(sid) {
            None => steps_removed.push(sid.to_string()),
            Some(cstep) => {
                if pstep.owner != cstep.owner {
                    owners_changed.push((sid.to_string(), pstep.owner, cstep.owner));
                }
                if pstep.agent_id != cstep.agent_id {
                    agents_changed.push((
                        sid.to_string(),
                        pstep.agent_id.clone(),
                        cstep.agent_id.clone(),
                    ));
                }
            }
        }
    }

    let parent_val: std::collections::HashMap<&str, &ValidationEntry> = parent
        .validation_policy
        .entries
        .iter()
        .map(|v| (v.validation_id.as_str(), v))
        .collect();
    let child_val: std::collections::HashMap<&str, &ValidationEntry> = child
        .validation_policy
        .entries
        .iter()
        .map(|v| (v.validation_id.as_str(), v))
        .collect();

    let mut validation_removed = Vec::new();
    let mut validation_weakened = Vec::new();
    for (vid, pve) in &parent_val {
        match child_val.get(vid) {
            None => validation_removed.push(vid.to_string()),
            Some(cve) => {
                if is_weaker(&cve.requirement, &pve.requirement) {
                    validation_weakened.push((
                        vid.to_string(),
                        pve.requirement,
                        cve.requirement,
                    ));
                }
            }
        }
    }

    // Sort every collected vector for deterministic output. The diff feeds an
    // operator-facing `diff_summary` and a timeline payload, so it MUST be
    // stable regardless of HashMap iteration order.
    steps_added.sort();
    steps_removed.sort();
    owners_changed.sort_by(|a, b| a.0.cmp(&b.0));
    agents_changed.sort_by(|a, b| a.0.cmp(&b.0));
    validation_removed.sort();
    validation_weakened.sort_by(|a, b| a.0.cmp(&b.0));

    let capability_delta =
        compute_capability_delta(&parent.capability_envelope, &child.capability_envelope);

    PlanEnvelopeDiff {
        steps_added,
        steps_removed,
        owners_changed,
        agents_changed,
        validation_removed,
        validation_weakened,
        capability_delta,
    }
}

/// True when `new` is a weaker validation requirement than `old`
/// (Required > Advisory > Waived). Strengthening (Waived→Required) is NOT
/// weaker, so it doesn't trigger re-gate.
fn is_weaker(new: &ValidationRequirement, old: &ValidationRequirement) -> bool {
    fn rank(r: &ValidationRequirement) -> u8 {
        match r {
            ValidationRequirement::Required => 2,
            ValidationRequirement::Advisory => 1,
            ValidationRequirement::Waived => 0,
        }
    }
    rank(new) < rank(old)
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
                status: StepStatus::Pending,
            }],
            validation_policy: ValidationPolicy::default(),
            capability_envelope: Vec::new(),
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

    fn plan_with(steps: Vec<PlanStep>, val: Vec<ValidationEntry>) -> PlanFrame {
        PlanFrame {
            plan_id: "p".into(),
            version: 1,
            parent_version: None,
            workflow_id: "wf".into(),
            root_session_id: "s".into(),
            title: "T".into(),
            objective: "O".into(),
            status: PlanStatus::Approved,
            steps,
            validation_policy: ValidationPolicy { entries: val },
            capability_envelope: Vec::new(),
            approved_by: None,
            approved_at: None,
            created_by_agent_id: "planner".into(),
            reason: None,
            created_at: "now".into(),
        }
    }

    fn step(id: &str, owner: StepOwner, agent: Option<&str>) -> PlanStep {
        PlanStep {
            step_id: id.into(),
            title: format!("step {id}"),
            owner,
            depends_on: vec![],
            agent_id: agent.map(str::to_string),
            notes: None,
            status: StepStatus::Pending,
        }
    }

    fn vent(id: &str, req: ValidationRequirement) -> ValidationEntry {
        ValidationEntry {
            validation_id: id.into(),
            title: format!("v {id}"),
            class: ValidationClass::CorrectnessCheck,
            requirement: req,
        }
    }

    #[test]
    fn diff_cosmetic_change_inherits() {
        // Only the objective/title/reason changes — no envelope touch.
        let parent = plan_with(vec![step("s1", StepOwner::Agent, None)], vec![]);
        let mut child = parent.clone();
        child.objective = "reworded objective".into();
        child.title = "new title".into();
        let diff = plan_envelope_diff(&parent, &child);
        assert!(diff.is_cosmetic_only(), "cosmetic should inherit: {:?}", diff);
        assert!(!diff.requires_regate());
        assert_eq!(diff.summary(), "no envelope change");
    }

    #[test]
    fn diff_added_step_requires_regate() {
        let parent = plan_with(vec![step("s1", StepOwner::Agent, None)], vec![]);
        let child = plan_with(
            vec![step("s1", StepOwner::Agent, None), step("s2", StepOwner::Agent, None)],
            vec![],
        );
        let diff = plan_envelope_diff(&parent, &child);
        assert!(diff.requires_regate());
        assert_eq!(diff.steps_added, vec!["s2".to_string()]);
        assert!(diff.summary().contains("+step s2"));
    }

    #[test]
    fn diff_owner_change_requires_regate() {
        let parent = plan_with(vec![step("s1", StepOwner::Agent, None)], vec![]);
        let child = plan_with(vec![step("s1", StepOwner::Operator, None)], vec![]);
        let diff = plan_envelope_diff(&parent, &child);
        assert!(diff.requires_regate());
        assert_eq!(diff.owners_changed, vec![("s1".to_string(), StepOwner::Agent, StepOwner::Operator)]);
    }

    #[test]
    fn diff_agent_change_requires_regate() {
        let parent = plan_with(vec![step("s1", StepOwner::Agent, Some("coder.default"))], vec![]);
        let child = plan_with(vec![step("s1", StepOwner::Agent, Some("researcher.default"))], vec![]);
        let diff = plan_envelope_diff(&parent, &child);
        assert!(diff.requires_regate());
        assert_eq!(diff.agents_changed.len(), 1);
    }

    #[test]
    fn diff_validation_weakening_requires_regate_but_strengthening_does_not() {
        let parent = plan_with(vec![], vec![vent("v1", ValidationRequirement::Required)]);
        // Strengthen: Required → (stays Required) — no change
        let same = plan_with(vec![], vec![vent("v1", ValidationRequirement::Required)]);
        assert!(plan_envelope_diff(&parent, &same).is_cosmetic_only());

        // Weaken: Required → Advisory ⇒ re-gate
        let weakened = plan_with(vec![], vec![vent("v1", ValidationRequirement::Advisory)]);
        let d = plan_envelope_diff(&parent, &weakened);
        assert!(d.requires_regate());
        assert_eq!(d.validation_weakened.len(), 1);

        // Strengthen: Advisory → Required does NOT re-gate (more safety).
        let adv = plan_with(vec![], vec![vent("v1", ValidationRequirement::Advisory)]);
        let strengthened = plan_with(vec![], vec![vent("v1", ValidationRequirement::Required)]);
        assert!(plan_envelope_diff(&adv, &strengthened).is_cosmetic_only());
    }

    #[test]
    fn diff_removed_step_and_validation_require_regate() {
        let parent = plan_with(
            vec![step("s1", StepOwner::Agent, None), step("s2", StepOwner::Agent, None)],
            vec![ent("v1", ValidationRequirement::Required), ent("v2", ValidationRequirement::Required)],
        );
        let child = plan_with(vec![step("s1", StepOwner::Agent, None)], vec![ent("v1", ValidationRequirement::Required)]);
        let d = plan_envelope_diff(&parent, &child);
        assert!(d.requires_regate());
        assert_eq!(d.steps_removed, vec!["s2".to_string()]);
        assert_eq!(d.validation_removed, vec!["v2".to_string()]);
    }

    #[test]
    fn diff_multi_item_output_is_deterministic() {
        // With several added steps + removed steps + weakened validations, the
        // collected vectors (and the operator-facing summary) MUST be sorted,
        // not dependent on HashMap iteration order. Run a few times — a
        // non-deterministic ordering would flake here against the asserted order.
        let parent = plan_with(
            vec![
                step("s_alpha", StepOwner::Agent, None),
                step("s_beta", StepOwner::Agent, None),
                step("s_gamma", StepOwner::Agent, None),
            ],
            vec![
                ent("v_one", ValidationRequirement::Required),
                ent("v_two", ValidationRequirement::Required),
            ],
        );
        let child = plan_with(
            // s_beta + s_gamma removed; s_delta + s_epsilon added
            vec![
                step("s_alpha", StepOwner::Agent, None),
                step("s_delta", StepOwner::Agent, None),
                step("s_epsilon", StepOwner::Agent, None),
            ],
            // both weakened Required → Advisory
            vec![
                ent("v_one", ValidationRequirement::Advisory),
                ent("v_two", ValidationRequirement::Advisory),
            ],
        );
        for _ in 0..8 {
            let d = plan_envelope_diff(&parent, &child);
            // Sorted lexicographically — not insertion order.
            assert_eq!(d.steps_added, vec!["s_delta".to_string(), "s_epsilon".to_string()]);
            assert_eq!(d.steps_removed, vec!["s_beta".to_string(), "s_gamma".to_string()]);
            assert_eq!(d.validation_weakened.iter().map(|(v, _, _)| v).collect::<Vec<_>>(),
                vec![&"v_one".to_string(), &"v_two".to_string()]);
            // Summary is stable too.
            let s = d.summary();
            assert!(s.contains("+step s_delta,s_epsilon"), "sorted added in summary: {s}");
            assert!(s.contains("−step s_beta,s_gamma"), "sorted removed in summary: {s}");
        }
    }

    // helper alias used above
    fn ent(id: &str, req: ValidationRequirement) -> ValidationEntry {
        vent(id, req)
    }

    #[test]
    fn diff_capability_broadening_requires_regate() {
        let parent = plan_with(vec![], vec![]);
        let mut child = parent.clone();
        child.capability_envelope = vec![Capability::NetworkAccess {
            hosts: vec!["api.example.com".to_string()],
        }];
        let diff = plan_envelope_diff(&parent, &child);
        assert!(diff.requires_regate());
        assert!(diff.capability_delta.has_broadening());
    }

    #[test]
    fn diff_capability_narrowing_does_not_require_regate() {
        let mut parent = plan_with(vec![], vec![]);
        parent.capability_envelope = vec![Capability::NetworkAccess {
            hosts: vec!["api.example.com".to_string(), "cdn.example.com".to_string()],
        }];
        let mut child = parent.clone();
        child.capability_envelope = vec![Capability::NetworkAccess {
            hosts: vec!["api.example.com".to_string()],
        }];
        let diff = plan_envelope_diff(&parent, &child);
        assert!(!diff.requires_regate());
        assert!(!diff.capability_delta.narrowed.is_empty());
    }
}
