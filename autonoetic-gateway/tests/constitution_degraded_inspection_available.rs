//! Constitution P-7.18 — Degraded session retains inspection tools.
//!
//! When a session is degraded the gateway revokes specialized capabilities
//! (sandbox_exec, web_*, agent_spawn, agent_revision_*, scheduler_*,
//! credential_*) but **keeps** the inspection allowlist
//! (observability_*, knowledge_search/read/search_by_tags, constitution_read,
//! resolve, execution_search, digest_query) so the agent can
//! diagnose its own state and report on it.
//!
//! Rationale: degraded mode without inspection is a Ri-0.5 spirit
//! violation — an agent that cannot see why it was degraded cannot
//! recover, report, or even read the rule that degraded it. The latest
//! session logs (2026-05-12) showed the evaluator unable to look up
//! its own degradation rule because the tool tier had been clamped too
//! aggressively. See issue #184 / Problem 2.

mod support;

use autonoetic_gateway::runtime::tool_dispatch::determine_tool_tier_filter;
use autonoetic_gateway::runtime::tools::{default_registry, ToolTierFilter};
use autonoetic_types::agent::{AgentIdentity, AgentManifest, RuntimeDeclaration, SessionState};
use autonoetic_types::capability::Capability;

fn high_privilege_manifest() -> AgentManifest {
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
            id: "test.degraded".to_string(),
            name: "Degraded Agent".to_string(),
            description: "Agent with most action capabilities, used to verify degraded clamp keeps inspection".to_string(),
        },
        capabilities: vec![
            Capability::CodeExecution {
                patterns: vec!["*".to_string()],
                commands: vec![],
            },
            Capability::NetworkAccess {
                hosts: vec!["*.example.com".to_string()],
            },
            Capability::AgentSpawn {
                max_children: 5,
                max_spawn_depth: 0,
            },
            Capability::AgentRevision {
                patterns: vec!["*".to_string()],
            },
            Capability::SchedulerAccess {
                patterns: vec!["*".to_string()],
            },
            Capability::CredentialAccess {
                services: vec!["*".to_string()],
            },
            Capability::WriteAccess {
                scopes: vec!["/tmp/*".to_string()],
            },
            Capability::ReadAccess {
                scopes: vec!["*".to_string()],
            },
        ],
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
fn degraded_filter_is_returned_for_degraded_session_state() {
    let manifest = high_privilege_manifest();
    let filter = determine_tool_tier_filter(
        &manifest,
        Some("root-session"),
        false,
        SessionState::Degraded,
        true,
    );
    assert!(
        filter.always_include_inspection_tools,
        "SessionState::Degraded must carry the inspection-tools flag so the agent can self-diagnose"
    );
}

#[test]
fn degraded_filter_blocks_specialized_action_tools() {
    let filter = ToolTierFilter::degraded();
    // Tier-filter scope: Specialized + Workflow tier action tools are blocked.
    // (sandbox_exec / artifact_exec are Core tier per config/tools.yaml; they
    // pass this filter but are blocked at the processor level via
    // `is_degraded_mode_tool_blocked` — covered by other tests.)
    for name in [
        "web_search",
        "web_fetch",
        "agent_spawn",
        "agent_revision_create",
        "agent_revision_promote",
        "promotion_create",
        "admin_proposal_submit",
    ] {
        assert!(
            !filter.allows(name),
            "degraded mode must block tool '{}' at the tier-filter level",
            name
        );
    }
}

#[test]
fn degraded_filter_keeps_core_write_tools() {
    let filter = ToolTierFilter::degraded();
    // Pre-existing degraded behaviour: Core tier is still allowed.
    // content_write is Core and must remain available (this is what
    // distinguishes degraded from clarification).
    assert!(
        filter.allows("content_write"),
        "degraded mode keeps Core tier tools — content_write must remain available"
    );
}

#[test]
fn degraded_filter_allows_inspection_tools_for_self_diagnosis() {
    let filter = ToolTierFilter::degraded();
    for name in [
        "observability_search",
        "observability_read",
        "observability_read_reasoning",
        "constitution_read",
        "knowledge_search",
        "knowledge_read",
        "resolve",
        "execution_search",
        "digest_query",
    ] {
        assert!(
            filter.allows(name),
            "degraded mode must keep inspection tool '{}' so the agent can self-diagnose",
            name
        );
    }
}

#[test]
fn degraded_filter_is_strictly_more_permissive_than_clarification() {
    let degraded = ToolTierFilter::degraded();
    let clarif = ToolTierFilter::clarification();

    // Clarification blocks content_write; degraded keeps it.
    assert!(degraded.allows("content_write"));
    assert!(!clarif.allows("content_write"));

    // Both block Specialized/Workflow action tools at the tier filter.
    for action in ["web_search", "agent_spawn", "agent_revision_create"] {
        assert!(!degraded.allows(action), "degraded must block '{}'", action);
        assert!(!clarif.allows(action), "clarification must block '{}'", action);
    }

    // Both allow inspection tools.
    for inspect in ["observability_search", "constitution_read", "knowledge_search"] {
        assert!(degraded.allows(inspect));
        assert!(clarif.allows(inspect));
    }
}

#[test]
fn degraded_filter_in_default_registry_exposes_inspection_but_not_actions() {
    let manifest = high_privilege_manifest();
    let registry = default_registry();
    let filter = ToolTierFilter::degraded();

    let visible: Vec<String> = registry
        .available_definitions_filtered(&manifest, Some(&filter))
        .into_iter()
        .map(|d| d.name)
        .collect();

    // Hard-block list (tier-filter scope): Specialized + Workflow tier actions
    // must not surface. (sandbox_exec / artifact_exec are Core tier and are
    // blocked at the processor level instead — see
    // `tool_call_processor::is_degraded_mode_tool_blocked`.)
    for forbidden in [
        "web_search",
        "agent_spawn",
        "agent_revision_create",
    ] {
        assert!(
            !visible.iter().any(|name| name == forbidden),
            "degraded registry exposed action tool '{}': {:?}",
            forbidden,
            visible
        );
    }

    // At least one inspection tool must be visible so the agent can
    // look up its own degradation rule.
    assert!(
        visible.iter().any(|n| n == "constitution_read"),
        "degraded registry must expose constitution_read so the agent can look up P-7.18: {:?}",
        visible
    );
    assert!(
        visible.iter().any(|n| n.starts_with("observability_")),
        "degraded registry must expose observability_* for self-diagnosis: {:?}",
        visible
    );
}
