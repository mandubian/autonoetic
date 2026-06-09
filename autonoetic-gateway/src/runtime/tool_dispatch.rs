//! Tool dispatch helpers: tier filtering, LoopGuard construction, and Ri-0.6
//! capability-narrowing checks.
//!
//! Extracted from `lifecycle.rs` so that the main execution loop remains
//! focused on turn orchestration while all tool-surface / guard wiring lives
//! in one place.

use autonoetic_types::agent::{AgentManifest, LoopGuardDeclaration};
use autonoetic_types::config::{GatewayConfig, LoopGuardConfig};
use std::path::Path;

use crate::runtime::guard::LoopGuard;

// ---------------------------------------------------------------------------
// Ri06CapabilitySnapshot
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Ri06CapabilitySnapshot {
    pub(crate) allowed_tier_names: Vec<String>,
    pub(crate) session_state: autonoetic_types::agent::SessionState,
}

impl Ri06CapabilitySnapshot {
    pub(crate) fn from_filter(
        filter: &crate::runtime::tools::ToolTierFilter,
        session_state: autonoetic_types::agent::SessionState,
    ) -> Self {
        use autonoetic_types::agent::ToolTier;
        let mut names: Vec<&'static str> = if filter.allowed_tiers.is_empty() {
            vec!["core", "workflow", "specialized"]
        } else {
            filter
                .allowed_tiers
                .iter()
                .map(|tier| match tier {
                    ToolTier::Core => "core",
                    ToolTier::Workflow => "workflow",
                    ToolTier::Specialized => "specialized",
                })
                .collect()
        };
        names.sort_unstable();
        names.dedup();
        Self {
            allowed_tier_names: names.into_iter().map(|s| s.to_string()).collect(),
            session_state,
        }
    }

    pub(crate) fn is_subset_of(&self, other: &Self) -> bool {
        self.allowed_tier_names
            .iter()
            .all(|tier| other.allowed_tier_names.contains(tier))
    }

    pub(crate) fn is_strict_subset_of(&self, other: &Self) -> bool {
        self.is_subset_of(other) && self.allowed_tier_names != other.allowed_tier_names
    }
}

// ---------------------------------------------------------------------------
// LoopGuard helpers
// ---------------------------------------------------------------------------

pub(crate) fn tool_result_counts_as_progress(result: &str) -> bool {
    if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(result) {
        if let Some(ok) = parsed.get("ok").and_then(|v| v.as_bool()) {
            return ok;
        }
        if let Some(approval_required) = parsed.get("approval_required").and_then(|v| v.as_bool()) {
            return !approval_required;
        }
        if let Some(exit_code) = parsed.get("exit_code").and_then(|v| v.as_i64()) {
            return exit_code == 0;
        }
        if parsed.get("error").is_some() || parsed.get("error_type").is_some() {
            return false;
        }
        return true;
    }
    false
}

pub(crate) fn load_manifest_loop_guard_declaration(agent_dir: &Path) -> Option<LoopGuardDeclaration> {
    let skill_path = agent_dir.join("SKILL.md");
    let skill = std::fs::read_to_string(skill_path).ok()?;
    let frontmatter = skill.split("---").nth(1)?;
    let root = serde_yaml::from_str::<serde_yaml::Value>(frontmatter).ok()?;

    let direct = root.get("loop_guard").cloned();
    let nested = root
        .get("metadata")
        .and_then(|m| m.get("autonoetic"))
        .and_then(|a| a.get("loop_guard"))
        .cloned();

    direct
        .or(nested)
        .and_then(|v| serde_yaml::from_value::<LoopGuardDeclaration>(v).ok())
}

pub(crate) fn effective_loop_guard_config(
    system: &LoopGuardConfig,
    declaration: Option<&LoopGuardDeclaration>,
) -> LoopGuardConfig {
    let Some(decl) = declaration else {
        return system.clone();
    };

    let mut effective = system.clone();
    if let Some(v) = decl.max_loops_without_progress {
        effective.max_loops_without_progress = v.min(system.max_loops_without_progress);
    }
    if let Some(v) = decl.max_tool_failures {
        effective.max_tool_failures = v.min(system.max_tool_failures);
    }
    if let Some(v) = decl.max_consecutive_same_progress {
        effective.max_consecutive_same_progress = v.min(system.max_consecutive_same_progress);
    }
    if let Some(v) = decl.max_child_failures {
        effective.max_child_failures = v.min(system.max_child_failures);
    }
    effective
}

pub(crate) fn loop_guard_from_config_and_manifest(config: Option<&GatewayConfig>, agent_dir: &Path) -> LoopGuard {
    match config {
        Some(cfg) => {
            let declaration = load_manifest_loop_guard_declaration(agent_dir);
            let effective = effective_loop_guard_config(&cfg.loop_guard, declaration.as_ref());
            LoopGuard::with_config(&effective)
        }
        None => LoopGuard::new(5),
    }
}

// ---------------------------------------------------------------------------
// Tool tier filtering
// ---------------------------------------------------------------------------

/// Determine the tool tier filter based on agent manifest configuration and
/// runtime workflow state.
///
/// Three inputs drive the filter:
///
/// 1. **Manifest-declared tiers**: agents can declare `allowed_tool_tiers` to
///    permanently restrict their tool surface.
/// 2. **Pending approvals**: when the session (or any session in the same root)
///    has pending approvals, the tool surface is narrowed to Core + Workflow
///    tiers with `always_include_approval_tools: true`. This prevents agents
///    from launching new specialized operations (web search, revision creation,
///    promotion) while waiting for human approval.
/// 3. **Child session handoff**: child agent sessions (session_id contains `/`)
///    get Core-only tools by default, plus the non-core tiers implied by their
///    manifest capabilities. Promotion-gate child agents (`auditor.default`,
///    `static_evaluator.default`, etc.) set
///    [`crate::runtime::tools::ToolTierFilter::allow_promotion_record_without_specialized_tier`]
///    so `promotion_record` (a Specialized-tier tool) is still visible without
///    exposing other specialized tools such as `web_search`.
///
/// Manifest-declared tiers always take precedence over runtime inference — if an
/// agent explicitly restricts itself, the restriction is honoured.
pub fn determine_tool_tier_filter(
    manifest: &AgentManifest,
    session_id: Option<&str>,
    has_pending_approvals: bool,
    session_state: autonoetic_types::agent::SessionState,
    tool_tier_escalated: bool,
) -> crate::runtime::tools::ToolTierFilter {
    if session_state == autonoetic_types::agent::SessionState::Clarification {
        return crate::runtime::tools::ToolTierFilter::clarification();
    }
    if session_state == autonoetic_types::agent::SessionState::Degraded {
        return crate::runtime::tools::ToolTierFilter::degraded();
    }

    if !manifest.allowed_tool_tiers.is_empty() {
        return crate::runtime::tools::ToolTierFilter {
            allowed_tiers: manifest.allowed_tool_tiers.clone(),
            always_include_approval_tools: true,
            always_include_inspection_tools: false,
            clarification_read_only: false,
            allow_promotion_record_without_specialized_tier: false,
        };
    }

    if has_pending_approvals {
        return crate::runtime::tools::ToolTierFilter::core_and_workflow_with_approvals();
    }

    let is_child = session_id.map(|sid| sid.contains('/')).unwrap_or(false);
    if is_child {
        return child_tool_tier_filter_for_manifest(manifest);
    }

    // Progressive disclosure: root sessions start with Core+Workflow.
    // Once escalated (agent attempted a Specialized tool), all tiers are available.
    if tool_tier_escalated {
        crate::runtime::tools::ToolTierFilter::all()
    } else {
        crate::runtime::tools::ToolTierFilter::core_and_workflow()
    }
}

pub(crate) fn child_tool_tier_filter_for_manifest(
    manifest: &AgentManifest,
) -> crate::runtime::tools::ToolTierFilter {
    use autonoetic_types::agent::ToolTier;
    use autonoetic_types::capability::Capability;

    let mut allowed_tiers = vec![ToolTier::Core];

    let needs_workflow = manifest.capabilities.iter().any(|c| {
        matches!(
            c,
            Capability::AgentSpawn { .. }
                | Capability::AgentMessage { .. }
                | Capability::SchedulerAccess { .. }
                | Capability::BackgroundReevaluation { .. }
                | Capability::ApprovalQueue { .. }
                | Capability::SchedulerSignal { .. }
                | Capability::Evaluation { .. }
        )
    });
    if needs_workflow {
        allowed_tiers.push(ToolTier::Workflow);
    }

    let needs_specialized = manifest.capabilities.iter().any(|c| {
        matches!(
            c,
            Capability::AgentRevision { .. }
                | Capability::ConstitutionalProposal { .. }
                | Capability::SkillInstall { .. }
                | Capability::CredentialAccess { .. }
                | Capability::UserProfileAccess { .. }
        )
    });
    if needs_specialized {
        allowed_tiers.push(ToolTier::Specialized);
    }

    crate::runtime::tools::ToolTierFilter {
        allowed_tiers,
        always_include_approval_tools: true,
        always_include_inspection_tools: false,
        clarification_read_only: false,
        allow_promotion_record_without_specialized_tier:
            crate::runtime::tools::promotion::manifest_may_record_promotion_verdicts(manifest),
    }
}

// ---------------------------------------------------------------------------
// AgentExecutor Ri-0.6 capability narrowing methods
// ---------------------------------------------------------------------------

use crate::runtime::lifecycle::AgentExecutor;

impl AgentExecutor {
    pub(crate) fn build_ri_0_6_capability_snapshot(&self) -> Ri06CapabilitySnapshot {
        let filter = determine_tool_tier_filter(
            &self.manifest,
            self.session_id.as_deref(),
            false,
            self.session_state,
            true,
        );
        Ri06CapabilitySnapshot::from_filter(&filter, self.session_state)
    }

    pub(crate) fn resolve_ri_0_6_narrowing_path(&self, session_id: &str) -> anyhow::Result<&'static str> {
        anyhow::ensure!(
            self.session_state == autonoetic_types::agent::SessionState::Degraded,
            "Ri-0.6 violation: capability narrowing requires degraded mode (session='{}')",
            session_id
        );
        let store = self.gateway_store.as_ref().ok_or_else(|| {
            anyhow::anyhow!(
                "Ri-0.6 violation: capability narrowing requires gateway store evidence (session='{}')",
                session_id
            )
        })?;
        let degraded_events: Vec<_> = store
            .search_causal_events(Some(session_id), None, 128)?
            .into_iter()
            .filter(|e| e.category == "session" && e.action == "session.degraded")
            .collect();
        anyhow::ensure!(
            !degraded_events.is_empty(),
            "Ri-0.6 violation: narrowing detected without session.degraded causal event (session='{}')",
            session_id
        );

        let mut saw_operator_source = false;
        for event in degraded_events {
            anyhow::ensure!(
                !event.enforced_rules.is_empty(),
                "Ri-0.6 violation: session.degraded event '{}' has no enforced rules",
                event.event_id
            );
            if let Some(payload_raw) = event.payload.as_deref() {
                let payload: serde_json::Value = serde_json::from_str(payload_raw).map_err(|e| {
                    anyhow::anyhow!(
                        "Ri-0.6 violation: session.degraded event '{}' has invalid JSON payload: {}",
                        event.event_id,
                        e
                    )
                })?;
                if payload
                    .get("source")
                    .and_then(|v| v.as_str())
                    .map(|s| s == "operator")
                    .unwrap_or(false)
                {
                    saw_operator_source = true;
                }
            }
        }

        Ok(if saw_operator_source {
            "operator_command"
        } else {
            "degraded_mode"
        })
    }

    pub(crate) fn check_ri_0_6_turn_snapshot(&mut self, session_id: &str, turn_id: &str) -> anyhow::Result<()> {
        let current = self.build_ri_0_6_capability_snapshot();
        let Some(previous) = self.ri_0_6_previous_snapshot.clone() else {
            self.ri_0_6_previous_snapshot = Some(current);
            return Ok(());
        };

        let current_subset_of_previous = current.is_subset_of(&previous);
        let previous_subset_of_current = previous.is_subset_of(&current);

        if current.is_strict_subset_of(&previous) {
            let narrowing_path = self.resolve_ri_0_6_narrowing_path(session_id)?;
            let store = self.gateway_store.as_ref().ok_or_else(|| {
                anyhow::anyhow!(
                    "Ri-0.6 violation: capability narrowing event could not be recorded (gateway store unavailable)"
                )
            })?;
            store.create_causal_event(&autonoetic_types::causal_chain::CausalEventRecord {
                event_id: format!("ri06-{}", uuid::Uuid::new_v4()),
                agent_id: self.manifest.agent.id.clone(),
                session_id: session_id.to_string(),
                turn_id: Some(turn_id.to_string()),
                event_seq: 0,
                timestamp: chrono::Utc::now().to_rfc3339(),
                category: "session".to_string(),
                action: "session.capability_narrowed".to_string(),
                status: "active".to_string(),
                enforced_rules: vec!["Ri-0.6".to_string()],
                target: None,
                payload: Some(
                    serde_json::json!({
                        "narrowing_path": narrowing_path,
                        "previous_allowed_tiers": previous.allowed_tier_names,
                        "current_allowed_tiers": current.allowed_tier_names,
                        "previous_session_state": previous.session_state,
                        "current_session_state": current.session_state,
                    })
                    .to_string(),
                ),
                payload_ref: None,
                evidence_ref: None,
                reason: None,
            })?;
        } else if !current_subset_of_previous && !previous_subset_of_current {
            anyhow::bail!(
                "Ri-0.6 violation: capability tier set changed outside subset/superset relation \
                 (session='{}', previous={:?}, current={:?})",
                session_id,
                previous.allowed_tier_names,
                current.allowed_tier_names
            );
        }

        self.ri_0_6_previous_snapshot = Some(current);
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod loop_guard_tests {
    use super::tool_result_counts_as_progress;

    #[test]
    fn test_tool_result_counts_as_progress_ok_true() {
        assert!(tool_result_counts_as_progress(r#"{"ok": true}"#));
    }

    #[test]
    fn test_tool_result_counts_as_progress_ok_false() {
        assert!(!tool_result_counts_as_progress(r#"{"ok": false}"#));
    }

    #[test]
    fn test_tool_result_counts_as_progress_approval_required_true() {
        assert!(!tool_result_counts_as_progress(
            r#"{"approval_required": true}"#
        ));
    }

    #[test]
    fn test_tool_result_counts_as_progress_approval_required_false() {
        assert!(tool_result_counts_as_progress(
            r#"{"approval_required": false}"#
        ));
    }

    #[test]
    fn test_tool_result_counts_as_progress_exit_code_zero() {
        assert!(tool_result_counts_as_progress(r#"{"exit_code": 0}"#));
    }

    #[test]
    fn test_tool_result_counts_as_progress_exit_code_nonzero() {
        assert!(!tool_result_counts_as_progress(r#"{"exit_code": 1}"#));
    }

    #[test]
    fn test_tool_result_counts_as_progress_error_field() {
        assert!(!tool_result_counts_as_progress(
            r#"{"error": "something went wrong"}"#
        ));
    }

    #[test]
    fn test_tool_result_counts_as_progress_error_type_field() {
        assert!(!tool_result_counts_as_progress(
            r#"{"error_type": "validation", "message": "bad input"}"#
        ));
    }

    #[test]
    fn test_tool_result_counts_as_progress_plain_data() {
        assert!(tool_result_counts_as_progress(
            r#"{"results": [], "count": 0}"#
        ));
    }

    #[test]
    fn test_tool_result_counts_as_progress_invalid_json() {
        assert!(!tool_result_counts_as_progress("not json"));
    }
}

#[cfg(test)]
mod tier_filter_tests {
    use super::determine_tool_tier_filter;
    use autonoetic_types::agent::{AgentIdentity, AgentManifest, RuntimeDeclaration, SessionState, ToolTier};

    fn test_manifest() -> AgentManifest {
        AgentManifest {
            version: "1.0".to_string(),
            runtime: RuntimeDeclaration {
                engine: "autonoetic".to_string(),
                gateway_version: "0.1.0".to_string(),
                sdk_version: "0.1.0".to_string(),
                runtime_type: "stateful".to_string(),
                sandbox: "bubblewrap".to_string(),
                runtime_lock: "runtime.lock".to_string(),
            },
            agent: AgentIdentity {
                id: "test-agent".to_string(),
                name: "test".to_string(),
                description: "test".to_string(),
            },
            capabilities: vec![],
            llm_overrides: None,
            llm_preset: None,
            llm_config: None,
            limits: None,
            background: None,
            disclosure: None,
            io: None,
            middleware: None,
            execution_mode: Default::default(),
            script_entry: None,
            script_input_mode: Default::default(),
            gateway_url: None,
            gateway_token: None,
            allowed_tool_tiers: vec![],
            agentskills_import: None,
            compression: None,
            sandbox_network: autonoetic_types::agent::SandboxNetworkPolicy::default(),
        }
    }

    #[test]
    fn test_root_session_no_pending_approvals_allows_all() {
        let manifest = test_manifest();
        let filter = determine_tool_tier_filter(&manifest, Some("root-session"), false, SessionState::Normal, true);
        assert!(filter.allows("content_write"));
        assert!(filter.allows("web_search"));
        assert!(filter.allows("agent_spawn"));
        assert!(filter.allows("promotion_record"));
    }

    #[test]
    fn test_child_session_core_only_by_default() {
        let manifest = test_manifest();
        let filter = determine_tool_tier_filter(&manifest, Some("root/child-session"), false, SessionState::Normal, true);
        assert!(filter.allows("content_write"));
        assert!(filter.allows("sandbox_exec"));
        assert!(!filter.allows("web_search"));
        assert!(!filter.allows("agent_spawn"));
        assert!(!filter.allows("promotion_record"));
    }

    #[test]
    fn test_child_promotion_federation_agent_allows_promotion_record_not_web_search() {
        let mut manifest = test_manifest();
        manifest.agent.id = "static_evaluator.default".to_string();
        let filter = determine_tool_tier_filter(
            &manifest,
            Some("root/child-static-eval"),
            false,
            SessionState::Normal,
            true,
        );
        assert!(filter.allows("promotion_record"));
        assert!(!filter.allows("web_search"));
    }

    #[test]
    fn test_child_auditor_allows_promotion_record_and_workflow_without_full_specialized() {
        use autonoetic_types::capability::Capability;
        let mut manifest = test_manifest();
        manifest.agent.id = "auditor.default".to_string();
        manifest.capabilities = vec![Capability::Evaluation {
            patterns: vec!["*".to_string()],
        }];
        let filter = determine_tool_tier_filter(
            &manifest,
            Some("root/child-auditor"),
            false,
            SessionState::Normal,
            true,
        );
        assert!(filter.allows("promotion_record"));
        assert!(filter.allows("workflow_wait"));
        assert!(!filter.allows("web_search"));
    }

    #[test]
    fn test_pending_approvals_restricts_to_core_and_workflow() {
        let manifest = test_manifest();
        let filter = determine_tool_tier_filter(&manifest, Some("root-session"), true, SessionState::Normal, true);
        assert!(filter.allows("content_write"));
        assert!(filter.allows("sandbox_exec"));
        assert!(filter.allows("agent_spawn"));
        assert!(filter.allows("approval_status"));
        assert!(filter.allows("workflow_state"));
        assert!(!filter.allows("web_search"));
        assert!(!filter.allows("promotion_record"));
        assert!(!filter.allows("agent_revision_create"));
    }

    #[test]
    fn test_manifest_declared_tiers_override_runtime_inference() {
        let mut manifest = test_manifest();
        manifest.allowed_tool_tiers = vec![ToolTier::Core, ToolTier::Specialized];
        let filter = determine_tool_tier_filter(&manifest, Some("root/child"), true, SessionState::Normal, true);
        assert!(filter.allows("content_write"));
        assert!(filter.allows("web_search"));
        assert!(!filter.allows("agent_spawn"));
        assert!(filter.allows("approval_status"));
    }

    #[test]
    fn test_no_session_id_allows_all() {
        let manifest = test_manifest();
        let filter = determine_tool_tier_filter(&manifest, None, false, SessionState::Normal, true);
        assert!(filter.allows("web_search"));
    }

    #[test]
    fn test_degraded_session_clamps_to_core_only() {
        let manifest = test_manifest();
        let filter = determine_tool_tier_filter(&manifest, Some("root-session"), false, SessionState::Degraded, true);
        assert!(filter.allows("content_write"), "core content tools allowed in degraded");
        assert!(filter.allows("sandbox_exec"), "sandbox_exec is core tier, allowed by tier filter");
        assert!(!filter.allows("web_search"), "web_search is specialized, blocked in degraded");
        assert!(!filter.allows("agent_spawn"), "agent_spawn is workflow, blocked in degraded");
        assert!(!filter.allows("promotion_record"), "promotion is specialized, blocked in degraded");
        assert!(!filter.allows("agent_revision_create"), "agent_revision is specialized, blocked in degraded");
    }

    #[test]
    fn test_degraded_overrides_manifest_declared_tiers() {
        let mut manifest = test_manifest();
        manifest.allowed_tool_tiers = vec![ToolTier::Core, ToolTier::Specialized];
        let filter = determine_tool_tier_filter(&manifest, Some("root-session"), false, SessionState::Degraded, true);
        assert!(filter.allows("content_write"), "core allowed");
        assert!(!filter.allows("web_search"), "specialized blocked despite manifest");
        assert!(!filter.allows("agent_spawn"), "workflow blocked");
    }
}
