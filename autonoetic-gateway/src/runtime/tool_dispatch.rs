//! Tool dispatch helpers: tier filtering, LoopGuard construction, and Ri-0.6
//! capability-narrowing checks.
//!
//! Extracted from `lifecycle.rs` so that the main execution loop remains
//! focused on turn orchestration while all tool-surface / guard wiring lives
//! in one place.

use autonoetic_types::agent::{AgentManifest, ExecutionMode, LoopGuardDeclaration};
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

/// Returns `true` when the tool result is a stagnant no-op — a successful
/// call that carries no new information and therefore should NOT reset the
/// loop-guard's no-progress counter.
///
/// Currently covers:
/// - `workflow_wait` with `waited_secs == 0` and `join_satisfied == false`
///   (probe returned "still running" — the agent already knew this)
/// - `planframe_amend` with `progress_recorded == false` and
///   `requires_regate == false` (a cosmetic-only amend that changed nothing
///   but title/objective/reason text — no step status moved, no envelope
///   expanded). Observed in `session-9d5b3ef1`: the planner re-sent the same
///   single step 11 times; every amend returned `ok: true` and reset the
///   no-progress counter, so `max_loops_without_progress` never tripped.
///   An amend that marks a step `completed` carries `progress_recorded: true`
///   and is NOT stagnant.
pub(crate) fn is_stagnant_poll(tool_name: &str, result: &str) -> bool {
    if tool_name == "workflow_wait" {
        if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(result) {
            let waited = parsed.get("waited_secs").and_then(|v| v.as_u64()).unwrap_or(u64::MAX);
            let satisfied = parsed.get("join_satisfied").and_then(|v| v.as_bool()).unwrap_or(false);
            let failed = parsed.get("any_failed").and_then(|v| v.as_bool()).unwrap_or(false);
            // A 0-second wait that didn't satisfy and didn't fail is a no-op probe.
            return waited == 0 && !satisfied && !failed;
        }
        return false;
    }
    if tool_name == "planframe_amend" {
        if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(result) {
            // Only a successful, cosmetic-only amend with no step-status
            // transition is stagnant. Envelope-expanding amends
            // (`requires_regate: true`) and progress-recording amends
            // (`progress_recorded: true`) reset the counter as usual.
            let ok = parsed.get("ok").and_then(|v| v.as_bool()).unwrap_or(false);
            let requires_regate = parsed
                .get("requires_regate")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let progress_recorded = parsed
                .get("progress_recorded")
                .and_then(|v| v.as_bool())
                // Default to true when absent so older shards / partial
                // results never get silently suppressed.
                .unwrap_or(true);
            return ok && !requires_regate && !progress_recorded;
        }
        return false;
    }
    false
}

/// Read-only, side-effect-free tools whose successful result advances no
/// workflow (#701). A successful call to one of these must NOT reset the
/// LoopGuard's no-progress counter — otherwise a planner can interleave one
/// read-only probe between every failed mutation and keep
/// `max_loops_without_progress` from ever tripping (observed in
/// `session-cc54cec3`, which wasted ~30 planner rounds this way).
///
/// This is the vetted subset observed in the death-spiral post-mortem plus the
/// obvious state-query tools (including roster directory reads). Being
/// conservative is deliberate: labelling a tool that actually mutates state as
/// read-only would let a real loop run unbounded, so only tools known to be
/// pure reads are listed.
///
/// Note: `resolve` is listed here because `resolve(include=metadata)` and
/// `resolve(include=files)` are pure probes. `resolve(include=content)` is
/// treated as substantive progress at the call site via
/// `is_resolve_content_read`.
pub(crate) fn is_read_only_tool(tool_name: &str) -> bool {
    matches!(
        tool_name,
        "resolve"
            | "workflow_state"
            | "planframe_get"
            | "planframe_list"
            | "planframe_history"
            | "approval_list"
            | "approval_status"
            | "agent_discover"
            | "agent_inspect"
            | "agent_list"
            | "artifact_inspect"
            | "session_peek"
            | "tool_discover"
            | "agent_revision_schema"
            | "promotion_query"
            | "knowledge_recall"
            | "knowledge_search"
            | "digest_query"
            | "observability_search"
            | "observability_read"
            | "observability_read_reasoning"
            | "execution_search"
    )
}

/// Returns true when a tool call is `resolve(include="content")`.
///
/// Content reads are substantive progress for review agents, so they should
/// reset the LoopGuard no-progress counter even though `resolve` is otherwise
/// classified as read-only (see `is_read_only_tool`). Metadata and files
/// resolves remain read-only probes.
pub(crate) fn is_resolve_content_read(tool_name: &str, arguments_json: &str) -> bool {
    if tool_name != "resolve" {
        return false;
    }
    serde_json::from_str::<serde_json::Value>(arguments_json)
        .ok()
        .and_then(|v| v.get("include").and_then(|x| x.as_str().map(|s| s == "content")))
        .unwrap_or(false)
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

pub(crate) fn loop_guard_from_config_and_manifest(
    config: Option<&GatewayConfig>,
    _agent_dir: &Path,
    declaration: Option<&LoopGuardDeclaration>,
    execution_mode: ExecutionMode,
) -> LoopGuard {
    match config {
        Some(cfg) => {
            // The declaration is loaded once at AgentExecutor construction
            // time. Do NOT re-read SKILL.md here — the YAML parse is
            // expensive and unsafe_libyaml 0.2.11 has a SIGSEGV bug in its
            // realloc path that can crash the process under certain inputs.
            let manifest_decl = declaration.cloned();
            if let Some(decl) = manifest_decl {
                let effective = effective_loop_guard_config(&cfg.loop_guard, Some(&decl));
                return LoopGuard::with_config(&effective);
            }

            // No declaration: derive role-aware defaults from execution_mode.
            // These may raise above the global system defaults so deterministic
            // executors (test runners, script agents) get headroom appropriate
            // to their shape.
            let role_default = LoopGuardDeclaration::for_execution_mode(execution_mode);
            let mut effective = cfg.loop_guard.clone();
            if let Some(v) = role_default.max_loops_without_progress {
                effective.max_loops_without_progress = v;
            }
            if let Some(v) = role_default.max_tool_failures {
                effective.max_tool_failures = v;
            }
            if let Some(v) = role_default.max_consecutive_same_progress {
                effective.max_consecutive_same_progress = v;
            }
            if let Some(v) = role_default.max_child_failures {
                effective.max_child_failures = v;
            }
            LoopGuard::with_config(&effective)
        }
        None => LoopGuard::new(5),
    }
}

/// Resolve per-agent `max_session_turns`, clamped to the system ceiling.
/// Returns the effective limit to use for this agent's sessions.
pub(crate) fn effective_max_session_turns(
    system_turns: u32,
    declaration: Option<&LoopGuardDeclaration>,
) -> u32 {
    match declaration.and_then(|d| d.max_session_turns) {
        Some(v) => v.min(system_turns),
        None => system_turns,
    }
}

/// Resolve the absolute per-session turn **hard cap** — the ceiling that
/// continuation approvals cannot lift (issue #854). Returns `0` (disabling the
/// hard cap in lockstep) whenever the *effective soft limit* is `0` — i.e. the
/// system soft limit is `0`, or a per-agent `max_session_turns` override
/// reduces it to `0`.
///
/// Resolution:
/// 1. The **system ceiling** is `system_hard` when configured, else
///    `2 × system_soft`; it is never allowed below `system_soft`.
/// 2. A per-agent `max_session_turns_hard` override is clamped *down* to that
///    ceiling (operator-controlled safety — an agent can tighten but never
///    loosen the ceiling).
/// 3. Absent a per-agent override, the agent's hard cap defaults to `2 ×` its
///    effective soft limit, itself clamped to the system ceiling.
/// 4. In all cases the hard cap is floored at the effective soft limit, so the
///    soft approval gate always has room to fire before the terminal trip.
pub(crate) fn effective_max_session_turns_hard(
    system_soft: u32,
    system_hard: Option<u32>,
    declaration: Option<&LoopGuardDeclaration>,
) -> u32 {
    let effective_soft = effective_max_session_turns(system_soft, declaration);
    // No soft limit ⇒ no hard cap. Covers both a disabled system limit
    // (`system_soft == 0`) and a per-agent override that clamps the effective
    // soft limit to 0. (`effective_soft > 0` also guarantees `system_soft > 0`,
    // so the ceiling arithmetic below is safe.)
    if effective_soft == 0 {
        return 0;
    }
    let system_ceiling = system_hard
        .unwrap_or_else(|| system_soft.saturating_mul(2))
        .max(system_soft);
    let agent_hard = match declaration.and_then(|d| d.max_session_turns_hard) {
        Some(v) => v.min(system_ceiling),
        None => effective_soft.saturating_mul(2).min(system_ceiling),
    };
    agent_hard.max(effective_soft)
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
        let mut filter = crate::runtime::tools::ToolTierFilter::degraded();
        // promotion_record is bookkeeping (writes a verdict to SQLite), not a
        // dangerous operation. Promotion-gate agents (auditor, static_evaluator,
        // unit_test_runner, sealed_evaluator) must be able to record their
        // verdicts even in degraded mode — otherwise the promotion pipeline
        // deadlocks (auditor can't record, builder can't promote, factory loops).
        // The promotion ACTION (agent_revision_promote) is still blocked because
        // it is Specialized tier and not exempted here.
        filter.allow_promotion_record_without_specialized_tier =
            crate::runtime::tools::promotion::manifest_may_record_promotion_verdicts(manifest);
        return filter;
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
                // CredentialAccess implies the `credential_*` tool family,
                // which is Workflow-tier (config/tools.yaml — "planner-first
                // orchestration, vault-side"). Without this arm the sole
                // licensed ceremony agent (`credential_onboarding.default`)
                // receives Core+Specialized but never the ceremony tools it
                // exists to run — found by the credential-register study
                // smoke (RFC classic-harness-usecase-validation §3.5).
                | Capability::CredentialAccess { .. }
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
mod tests {
    use super::{
        child_tool_tier_filter_for_manifest, is_resolve_content_read, is_stagnant_poll,
        tool_result_counts_as_progress,
    };
    use autonoetic_types::agent::{AgentManifest, ToolTier};
    use autonoetic_types::capability::Capability;

    #[test]
    fn credential_access_child_gets_workflow_tier_for_credential_tools() {
        // The ceremony agent's manifest shape: CredentialAccess only (plus
        // network/storage). `credential_*` tools are Workflow-tier, so the
        // child filter must include Workflow or the licensed ceremony agent
        // cannot run its own ceremony (credential-register study finding).
        let manifest = AgentManifest {
            capabilities: vec![
                Capability::CredentialAccess {
                    services: vec!["*".to_string()],
                },
                Capability::NetworkAccess {
                    hosts: vec!["*".to_string()],
                },
            ],
            ..AgentManifest::default()
        };
        let filter = child_tool_tier_filter_for_manifest(&manifest);
        assert!(
            filter.allowed_tiers.contains(&ToolTier::Workflow),
            "CredentialAccess child must see Workflow-tier (credential_*) tools, got {:?}",
            filter.allowed_tiers
        );
    }

    #[test]
    fn plain_child_stays_core_tier() {
        let manifest = AgentManifest {
            capabilities: vec![Capability::ReadAccess {
                scopes: vec!["self.*".to_string()],
            }],
            ..AgentManifest::default()
        };
        let filter = child_tool_tier_filter_for_manifest(&manifest);
        assert!(!filter.allowed_tiers.contains(&ToolTier::Workflow));
        assert!(!filter.allowed_tiers.contains(&ToolTier::Specialized));
    }

    #[test]
    fn resolve_content_read_not_read_only() {
        assert!(is_resolve_content_read(
            "resolve",
            r#"{"ref": "ar.x", "include": "content"}"#
        ));
    }

    #[test]
    fn resolve_metadata_read_is_read_only() {
        assert!(!is_resolve_content_read(
            "resolve",
            r#"{"ref": "ar.x", "include": "metadata"}"#
        ));
    }

    #[test]
    fn resolve_files_read_is_read_only() {
        assert!(!is_resolve_content_read(
            "resolve",
            r#"{"ref": "ar.x", "include": "files"}"#
        ));
    }

    #[test]
    fn resolve_without_include_is_read_only() {
        assert!(!is_resolve_content_read("resolve", r#"{"ref": "ar.x"}"#));
    }

    #[test]
    fn resolve_content_malformed_args_fails_closed() {
        assert!(!is_resolve_content_read("resolve", "not-json"));
    }

    #[test]
    fn non_resolve_tool_is_not_content_read() {
        assert!(!is_resolve_content_read("workflow_state", r#"{}"#));
    }

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

    // ── is_stagnant_poll: planframe_amend no-op detection ───────────────
    // Regression for session-9d5b3ef1's amendment loop. A cosmetic-only
    // amend (`progress_recorded: false`, `requires_regate: false`) is
    // stagnant and must NOT reset the no-progress counter; an amend that
    // records a step-status transition or expands the envelope is real
    // progress and resets as usual.

    #[test]
    fn stagnant_planframe_amend_cosmetic_no_progress() {
        let result = r#"{"ok":true,"inherited":true,"requires_regate":false,"progress_recorded":false,"grants_revoked":0,"diff_summary":"no envelope change"}"#;
        assert!(
            is_stagnant_poll("planframe_amend", result),
            "cosmetic amend with no step-status change must be stagnant"
        );
    }

    #[test]
    fn not_stagnant_planframe_amend_records_step_status() {
        // Marking a step completed is real progress — reset the counter.
        let result = r#"{"ok":true,"inherited":true,"requires_regate":false,"progress_recorded":true,"grants_revoked":0,"diff_summary":"no envelope change"}"#;
        assert!(
            !is_stagnant_poll("planframe_amend", result),
            "amend that recorded a step-status change must NOT be stagnant"
        );
    }

    #[test]
    fn not_stagnant_planframe_amend_envelope_expanded() {
        // Adding a step expands the envelope → requires_regate → not stagnant.
        let result = r#"{"ok":true,"inherited":false,"requires_regate":true,"progress_recorded":false,"grants_revoked":0,"diff_summary":"+step s2"}"#;
        assert!(
            !is_stagnant_poll("planframe_amend", result),
            "envelope-expanding amend must NOT be stagnant"
        );
    }

    #[test]
    fn not_stagnant_planframe_amend_failed() {
        // A failed amend (ok:false) is a failure, not a stagnant success —
        // it must not be suppressed here (the failure path handles it).
        let result = r#"{"ok":false,"error_type":"validation","message":"bad steps"}"#;
        assert!(
            !is_stagnant_poll("planframe_amend", result),
            "failed amend must not be classified as stagnant"
        );
    }

    #[test]
    fn stagnant_planframe_amend_defaults_progress_recorded_to_true_when_absent() {
        // Backward safety: a result missing `progress_recorded` must NOT be
        // suppressed (default to non-stagnant) so older shards never get
        // silently masked.
        let result = r#"{"ok":true,"inherited":true,"requires_regate":false,"grants_revoked":0}"#;
        assert!(
            !is_stagnant_poll("planframe_amend", result),
            "missing progress_recorded must default to NOT stagnant"
        );
    }

    #[test]
    fn workflow_wait_stagnant_still_detected_after_generalization() {
        // The original workflow_wait stagnant-poll path still works.
        let result = r#"{"ok":true,"waited_secs":0,"join_satisfied":false,"any_failed":false}"#;
        assert!(is_stagnant_poll("workflow_wait", result));
        let result = r#"{"ok":true,"waited_secs":5,"join_satisfied":false,"any_failed":false}"#;
        assert!(!is_stagnant_poll("workflow_wait", result));
    }

    #[test]
    fn unrelated_tool_is_never_stagnant() {
        let result = r#"{"ok":true,"progress_recorded":false,"requires_regate":false}"#;
        assert!(!is_stagnant_poll("content_write", result));
        assert!(!is_stagnant_poll("agent_spawn", result));
    }
}

#[cfg(test)]
mod tier_filter_tests {
    use super::determine_tool_tier_filter;
    use autonoetic_types::agent::{AgentIdentity, AgentManifest, RuntimeDeclaration, SessionState, ToolTier};

    fn test_manifest() -> AgentManifest {
        AgentManifest {
            remote_access: None,
            messaging: None,
            version: "1.0".to_string(),
            runtime: RuntimeDeclaration {
                mounts: Vec::new(),
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
            singleton: false,
            resident_idle_ttl_secs: None,
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
            adapter: None,
            execution_mode: Default::default(),
            script_entry: None,
            script_input_mode: Default::default(),
            gateway_url: None,
            gateway_token: None,
            allowed_tool_tiers: vec![],
            excluded_tools: vec![],
            sections: Vec::new(),
            agentskills_import: None,
            compression: None,
            open_web: false,
            sandbox_network: autonoetic_types::agent::SandboxNetworkPolicy::default(),
            egress: None,
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
        assert!(!filter.allows("promotion_record"), "promotion_record blocked for non-promotion-gate agent in degraded");
        assert!(!filter.allows("agent_revision_create"), "agent_revision is specialized, blocked in degraded");
    }

    #[test]
    fn test_degraded_allows_promotion_record_for_promotion_gate_agents() {
        let mut manifest = test_manifest();
        manifest.agent.id = "auditor.default".to_string();
        let filter = determine_tool_tier_filter(
            &manifest,
            Some("root/child-auditor"),
            false,
            SessionState::Degraded,
            true,
        );
        assert!(filter.allows("promotion_record"), "auditor must record verdicts even in degraded mode");
        assert!(filter.allows("content_write"), "core tools still available");
        assert!(!filter.allows("web_search"), "other specialized tools still blocked");
        assert!(!filter.allows("agent_revision_promote"), "promotion action still blocked");
    }

    #[test]
    fn test_degraded_overrides_manifest_declared_tiers() {
        let mut manifest = test_manifest();
        manifest.allowed_tool_tiers = vec![ToolTier::Core, ToolTier::Specialized];
        let filter = determine_tool_tier_filter(&manifest,
            Some("root-session"),
            false,
            SessionState::Degraded,
            true,
        );
        assert!(filter.allows("content_write"), "core allowed");
        assert!(!filter.allows("web_search"), "specialized blocked despite manifest");
        assert!(!filter.allows("agent_spawn"), "workflow blocked");
    }
}

#[cfg(test)]
mod loop_guard_tests {
    use super::loop_guard_from_config_and_manifest;
    use super::effective_max_session_turns_hard;
    use autonoetic_types::agent::ExecutionMode;
    use std::path::Path;

    #[test]
    fn reasoning_mode_uses_system_loop_guard_defaults() {
        let cfg = autonoetic_types::config::GatewayConfig::default();
        let guard = loop_guard_from_config_and_manifest(
            Some(&cfg),
            Path::new("/no-such-agent-dir"),
            None,
            ExecutionMode::Reasoning,
        );
        assert_eq!(
            guard.max_loops_without_progress,
            cfg.loop_guard.max_loops_without_progress
        );
        assert_eq!(guard.max_tool_failures, cfg.loop_guard.max_tool_failures);
    }

    #[test]
    fn script_mode_uses_role_aware_loop_guard_profile() {
        let cfg = autonoetic_types::config::GatewayConfig::default();
        let guard = loop_guard_from_config_and_manifest(
            Some(&cfg),
            Path::new("/no-such-agent-dir"),
            None,
            ExecutionMode::Script,
        );
        assert_eq!(guard.max_loops_without_progress, 15);
        assert_eq!(guard.max_tool_failures, 12);
    }

    #[test]
    fn manifest_loop_guard_declaration_overrides_role_profile() {
        let cfg = autonoetic_types::config::GatewayConfig::default();
        let declaration = autonoetic_types::agent::LoopGuardDeclaration {
            max_loops_without_progress: Some(2),
            max_tool_failures: Some(3),
            max_consecutive_same_progress: None,
            max_child_failures: None,
            max_session_turns: None,
            max_session_turns_hard: None,
        };
        let guard = loop_guard_from_config_and_manifest(
            Some(&cfg),
            Path::new("/no-such-agent-dir"),
            Some(&declaration),
            ExecutionMode::Script,
        );
        assert_eq!(guard.max_loops_without_progress, 2);
        assert_eq!(guard.max_tool_failures, 3);
    }

    fn decl(
        soft: Option<u32>,
        hard: Option<u32>,
    ) -> autonoetic_types::agent::LoopGuardDeclaration {
        autonoetic_types::agent::LoopGuardDeclaration {
            max_session_turns: soft,
            max_session_turns_hard: hard,
            ..Default::default()
        }
    }

    #[test]
    fn hard_cap_defaults_to_twice_system_soft_when_unset() {
        // No declaration, no system hard override ⇒ 2× the system soft limit.
        assert_eq!(effective_max_session_turns_hard(25, None, None), 50);
    }

    #[test]
    fn hard_cap_honours_explicit_system_ceiling() {
        // System hard override is used verbatim as the ceiling (≥ soft).
        assert_eq!(effective_max_session_turns_hard(25, Some(30), None), 30);
        // A configured ceiling below the soft limit is floored at the soft limit
        // so the soft gate always has room to fire first.
        assert_eq!(effective_max_session_turns_hard(25, Some(10), None), 25);
    }

    #[test]
    fn hard_cap_defaults_to_twice_effective_soft_for_per_agent_soft_override() {
        // Issue #854 researcher case: per-agent soft=20 under system soft=25 ⇒
        // default hard = 2×20 = 40 (≤ system ceiling of 2×25 = 50).
        let d = decl(Some(20), None);
        assert_eq!(effective_max_session_turns_hard(25, None, Some(&d)), 40);
    }

    #[test]
    fn per_agent_hard_override_is_clamped_down_to_system_ceiling() {
        // Agent asks for 200 but the system ceiling (2×25=50) caps it.
        let d = decl(None, Some(200));
        assert_eq!(effective_max_session_turns_hard(25, None, Some(&d)), 50);
        // Under an explicit system ceiling, the agent is clamped to it.
        let d2 = decl(None, Some(200));
        assert_eq!(effective_max_session_turns_hard(25, Some(60), Some(&d2)), 60);
    }

    #[test]
    fn per_agent_hard_override_can_tighten_but_is_floored_at_soft() {
        // A tighter-than-default hard cap is honoured...
        let d = decl(None, Some(30));
        assert_eq!(effective_max_session_turns_hard(25, None, Some(&d)), 30);
        // ...but never drops below the effective soft limit.
        let d2 = decl(Some(20), Some(5));
        assert_eq!(effective_max_session_turns_hard(25, None, Some(&d2)), 20);
    }

    #[test]
    fn hard_cap_disabled_when_soft_limit_disabled() {
        assert_eq!(effective_max_session_turns_hard(0, None, None), 0);
        assert_eq!(effective_max_session_turns_hard(0, Some(100), None), 0);
    }
}
