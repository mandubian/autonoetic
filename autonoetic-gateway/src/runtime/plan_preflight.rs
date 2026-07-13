//! RFC #777 Part C — Plan-time capability preflight.
//!
//! Deterministic check over a PlanFrame before execution: for each step, does
//! the intended agent declare the capabilities the step needs? Uncovered steps
//! produce advisory warnings — a plan may legitimately include steps whose
//! executor will be *built* (agent-factory ladder), so the preflight warns
//! with structure rather than blocks; proceeding past a warning is on the
//! record.
//!
//! Precedent: `artifact_prepare` does exactly this one-pass preflight for
//! credentials + network domains. This lifts the pattern from artifacts to
//! plans.
//!
//! Purely static — declared capabilities vs. declared step needs — so the
//! Lawful Executor gains nothing to judge (invariant 5: check existence, not
//! truth).

use autonoetic_types::plan_frame::PlanFrame;
use serde::{Deserialize, Serialize};

/// One step-level finding from the preflight.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PreflightFinding {
    pub step_id: String,
    pub agent_id: String,
    pub kind: PreflightKind,
    /// The capability type names that are not covered. Populated for both
    /// `UncoveredCapabilities` (agent exists but lacks some) and
    /// `AgentNotInstalled` (all required are uncovered). Empty for `Covered`.
    #[serde(default)]
    pub uncovered: Vec<String>,
    /// Human-readable detail for the operator/planner.
    pub detail: String,
}

/// What the preflight found for a step.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PreflightKind {
    /// The intended agent is not installed (may be built later — advisory).
    AgentNotInstalled,
    /// The agent exists but does not declare all required capabilities.
    UncoveredCapabilities,
    /// All declared capabilities are covered by the intended agent.
    Covered,
}

/// Result of running the preflight over a plan.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PreflightResult {
    pub plan_id: String,
    pub plan_version: u32,
    /// Total steps checked (steps with `required_capabilities` non-empty
    /// AND `agent_id` set).
    pub steps_checked: usize,
    pub findings: Vec<PreflightFinding>,
    /// Whether any finding is advisory (non-Covered). When `false`, every
    /// checked step has full capability coverage.
    pub has_warnings: bool,
}

impl PreflightResult {
    pub fn is_clean(&self) -> bool {
        !self.has_warnings
    }
}

/// Trait for looking up an agent's declared capabilities. The gateway
/// implements this against the agent registry; tests implement it with
/// a static map. Kept as a trait so the preflight logic is testable
/// without a running gateway.
pub trait CapabilityLookup {
    /// Returns the capability *type names* declared by the agent
    /// (e.g. `["NetworkAccess", "WriteAccess"]`), or `None` if the agent
    /// is not installed.
    fn declared_capabilities(&self, agent_id: &str) -> Option<Vec<String>>;
}

/// Run the preflight over a plan. Purely static — no LLM calls, no network
/// fetches, no hidden branches (I-10).
///
/// For each step where:
/// - `required_capabilities` is non-empty, AND
/// - `agent_id` is set
///
/// the preflight checks whether the agent exists and declares all required
/// capability types. Missing agents and uncovered capabilities produce
/// advisory findings.
pub fn preflight_plan<L: CapabilityLookup>(
    plan: &PlanFrame,
    lookup: &L,
) -> PreflightResult {
    let mut findings = Vec::new();
    let mut steps_checked = 0usize;

    for step in &plan.steps {
        if step.required_capabilities.is_empty() {
            continue;
        }
        let Some(ref agent_id) = step.agent_id else {
            continue;
        };
        steps_checked += 1;

        match lookup.declared_capabilities(agent_id) {
            None => {
                findings.push(PreflightFinding {
                    step_id: step.step_id.clone(),
                    agent_id: agent_id.clone(),
                    kind: PreflightKind::AgentNotInstalled,
                    uncovered: step.required_capabilities.clone(),
                    detail: format!(
                        "Step '{}' declares required capabilities {:?} but its intended agent '{}' is not installed. \
                        If this agent will be built by agent-factory, this warning is expected; otherwise, \
                        install or re-delegate.",
                        step.step_id, step.required_capabilities, agent_id
                    ),
                });
            }
            Some(declared) => {
                let uncovered: Vec<String> = step
                    .required_capabilities
                    .iter()
                    .filter(|req| !declared.iter().any(|d| d == *req))
                    .cloned()
                    .collect();
                if uncovered.is_empty() {
                    findings.push(PreflightFinding {
                        step_id: step.step_id.clone(),
                        agent_id: agent_id.clone(),
                        kind: PreflightKind::Covered,
                        uncovered: vec![],
                        detail: format!(
                            "Step '{}' capabilities covered by '{}'.",
                            step.step_id, agent_id
                        ),
                    });
                } else {
                    findings.push(PreflightFinding {
                        step_id: step.step_id.clone(),
                        agent_id: agent_id.clone(),
                        kind: PreflightKind::UncoveredCapabilities,
                        uncovered: uncovered.clone(),
                        detail: format!(
                            "Step '{}' requires {:?} but agent '{}' only declares {:?}. \
                            Missing: {:?}. Re-delegate, revise the plan, or escalate.",
                            step.step_id, step.required_capabilities, agent_id, declared, uncovered
                        ),
                    });
                }
            }
        }
    }

    let has_warnings = findings
        .iter()
        .any(|f| f.kind != PreflightKind::Covered);

    PreflightResult {
        plan_id: plan.plan_id.clone(),
        plan_version: plan.version,
        steps_checked,
        findings,
        has_warnings,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use autonoetic_types::plan_frame::{PlanFrame, PlanStep, StepOwner};
    use std::collections::HashMap;

    /// Test-only lookup: static agent_id → capability type names.
    struct StaticLookup(HashMap<String, Vec<String>>);

    impl CapabilityLookup for StaticLookup {
        fn declared_capabilities(&self, agent_id: &str) -> Option<Vec<String>> {
            self.0.get(agent_id).cloned()
        }
    }

    fn step(id: &str, agent_id: Option<&str>, caps: &[&str]) -> PlanStep {
        PlanStep {
            step_id: id.to_string(),
            title: id.to_string(),
            owner: StepOwner::default(),
            depends_on: vec![],
            agent_id: agent_id.map(str::to_string),
            notes: None,
            status: Default::default(),
            required_capabilities: caps.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn plan(steps: Vec<PlanStep>) -> PlanFrame {
        PlanFrame {
            plan_id: "plan-1".to_string(),
            version: 1,
            parent_version: None,
            workflow_id: "wf-1".to_string(),
            root_session_id: "root".to_string(),
            title: "test".to_string(),
            objective: "test".to_string(),
            status: Default::default(),
            steps,
            validation_policy: Default::default(),
            capability_envelope: vec![],
            approved_by: None,
            approved_at: None,
            created_by_agent_id: "planner.default".to_string(),
            reason: None,
            created_at: "2026-07-12T00:00:00Z".to_string(),
            expires_at: None,
        }
    }

    #[test]
    fn clean_when_all_covered() {
        let lookup = StaticLookup(
            [("coder.default".to_string(), vec!["WriteAccess".to_string(), "CodeExecution".to_string()])]
                .into_iter()
                .collect(),
        );
        let p = plan(vec![step("s1", Some("coder.default"), &["WriteAccess", "CodeExecution"])]);
        let result = preflight_plan(&p, &lookup);
        assert!(result.is_clean());
        assert_eq!(result.steps_checked, 1);
        assert_eq!(result.findings.len(), 1);
        assert_eq!(result.findings[0].kind, PreflightKind::Covered);
    }

    #[test]
    fn warns_on_uncovered_capability() {
        let lookup = StaticLookup(
            [("coder.default".to_string(), vec!["WriteAccess".to_string()])]
                .into_iter()
                .collect(),
        );
        let p = plan(vec![step("s1", Some("coder.default"), &["WriteAccess", "NetworkAccess"])]);
        let result = preflight_plan(&p, &lookup);
        assert!(!result.is_clean());
        assert_eq!(result.findings[0].kind, PreflightKind::UncoveredCapabilities);
        assert_eq!(result.findings[0].uncovered, vec!["NetworkAccess"]);
    }

    #[test]
    fn warns_on_agent_not_installed() {
        let lookup = StaticLookup(HashMap::new());
        let p = plan(vec![step("s1", Some("future.agent"), &["NetworkAccess"])]);
        let result = preflight_plan(&p, &lookup);
        assert!(!result.is_clean());
        assert_eq!(result.findings[0].kind, PreflightKind::AgentNotInstalled);
        assert_eq!(result.findings[0].uncovered, vec!["NetworkAccess"]);
    }

    #[test]
    fn skips_steps_without_required_caps() {
        let lookup = StaticLookup(HashMap::new());
        let p = plan(vec![PlanStep {
            required_capabilities: vec![],
            ..step("s1", Some("coder.default"), &[])
        }]);
        let result = preflight_plan(&p, &lookup);
        assert!(result.is_clean());
        assert_eq!(result.steps_checked, 0);
    }

    #[test]
    fn skips_steps_without_agent_id() {
        let lookup = StaticLookup(HashMap::new());
        let p = plan(vec![step("s1", None, &["NetworkAccess"])]);
        let result = preflight_plan(&p, &lookup);
        assert!(result.is_clean());
        assert_eq!(result.steps_checked, 0);
    }

    #[test]
    fn mixed_plan_reports_warnings_only_for_uncovered() {
        let lookup = StaticLookup(
            [
                ("coder.default".to_string(), vec!["WriteAccess".to_string(), "CodeExecution".to_string()]),
                ("researcher.default".to_string(), vec!["ReadAccess".to_string()]),
            ]
            .into_iter()
            .collect(),
        );
        let p = plan(vec![
            step("s1", Some("coder.default"), &["WriteAccess"]),           // covered
            step("s2", Some("researcher.default"), &["NetworkAccess"]),    // uncovered
            step("s3", Some("missing.agent"), &["ReadAccess"]),            // not installed
        ]);
        let result = preflight_plan(&p, &lookup);
        assert!(!result.is_clean());
        assert_eq!(result.steps_checked, 3);
        assert_eq!(result.findings.len(), 3);
        assert_eq!(result.findings[0].kind, PreflightKind::Covered);
        assert_eq!(result.findings[1].kind, PreflightKind::UncoveredCapabilities);
        assert_eq!(result.findings[2].kind, PreflightKind::AgentNotInstalled);
    }
}
