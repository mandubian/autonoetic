//! Constitution test for the clarification child session primitive (#172).
//!
//! When `SessionState::Clarification` is set the tool tier filter must clamp
//! the agent's tool surface to inspection-only, regardless of what the
//! manifest's capabilities or declared tool tiers would otherwise allow.
//!
//! This is the structural guarantee behind `gateway approvals ask-agent`:
//! an operator probe cannot trigger an action even if the agent is willing
//! or the system prompt is bypassed. See
//! `docs/design/human-gate-unification-plan.md` §Phase 5.


use autonoetic_gateway::runtime::tool_dispatch::determine_tool_tier_filter;
use autonoetic_gateway::runtime::tools::{default_registry, ToolTierFilter};
use autonoetic_types::agent::{AgentIdentity, AgentManifest, SessionState};
use autonoetic_types::capability::Capability;
use crate::support::manifest_builder::TestManifest;

fn high_privilege_manifest() -> AgentManifest {
    AgentManifest {
        agent: AgentIdentity {
            id: "test.high-priv".to_string(),
            name: "High-Privilege Agent".to_string(),
            description: "Agent with most action capabilities, used to verify clarification clamp".to_string(),
            singleton: false,
            resident_idle_ttl_secs: None,
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
        ..TestManifest::new().build()
    }
}

#[test]
fn clarification_filter_is_returned_for_clarification_session_state() {
    let manifest = high_privilege_manifest();
    let filter = determine_tool_tier_filter(
        &manifest,
        Some("root/test.high-priv-clarif-abcd1234"),
        false,
        SessionState::Clarification,
        true,
    );
    assert!(
        filter.clarification_read_only,
        "SessionState::Clarification must return a read-only filter"
    );
}

#[test]
fn clarification_filter_denies_action_tools_even_with_high_privilege_manifest() {
    let filter = ToolTierFilter::clarification();

    // The whole point of the clamp: actions are blocked regardless of capability.
    let blocked = [
        "sandbox_exec",
        "sandbox_layer_mount",
        "web_search",
        "web_fetch",
        "agent_spawn",
        "agent_message",
        "agent_revision_create",
        "agent_revision_promote",
        "scheduler_create",
        "scheduler_trigger",
        "credential_setup",
        "credential_use",
        "content_write",
        "skill_install",
        "approval_answer",
        "user_interaction_answer",
        "constitution_propose_amendment",
    ];
    for name in blocked {
        assert!(
            !filter.allows(name),
            "clarification tier must block action tool '{}'",
            name
        );
    }
}

#[test]
fn clarification_filter_allows_inspection_tools() {
    let filter = ToolTierFilter::clarification();
    let allowed = [
        "observability_search",
        "observability_read",
        "observability_read_reasoning",
        "constitution_read",
        "knowledge_search",
        "knowledge_read",
        "resolve",
        "execution_search",
        "digest_query",
    ];
    for name in allowed {
        assert!(
            filter.allows(name),
            "clarification tier must allow inspection tool '{}'",
            name
        );
    }
}

#[test]
fn clarification_filter_blocks_action_tools_from_default_registry() {
    let manifest = high_privilege_manifest();
    let registry = default_registry();
    let filter = ToolTierFilter::clarification();

    let visible: Vec<String> = registry
        .available_definitions_filtered(&manifest, Some(&filter))
        .into_iter()
        .map(|d| d.name)
        .collect();

    // Hard-block list — any of these slipping through is a structural Ri-0
    // (specifically Ri-0.6) violation for the clarification primitive.
    for forbidden in [
        "sandbox_exec",
        "web_search",
        "agent_spawn",
        "agent_revision_create",
        "scheduler_create",
        "credential_setup",
        "content_write",
        "skill_install",
    ] {
        assert!(
            !visible.iter().any(|name| name == forbidden),
            "default_registry exposed action tool '{}' to a clarification session: {:?}",
            forbidden,
            visible
        );
    }

    // The registry should still expose at least one inspection tool to make
    // the clarification session useful.
    assert!(
        visible.iter().any(|n| n.starts_with("observability_") || n.starts_with("knowledge_") || n == "constitution_read"),
        "default_registry exposed no inspection tools to clarification session: {:?}",
        visible
    );
}

#[test]
fn clarification_filter_distinct_from_degraded_and_normal() {
    let manifest = high_privilege_manifest();

    let normal = determine_tool_tier_filter(&manifest, None, false, SessionState::Normal, true);
    assert!(
        !normal.clarification_read_only,
        "SessionState::Normal must not set the clarification clamp"
    );

    let degraded = determine_tool_tier_filter(&manifest, None, false, SessionState::Degraded, true);
    assert!(
        !degraded.clarification_read_only,
        "SessionState::Degraded uses core_only, not the clarification clamp"
    );

    // Degraded should still allow non-inspection core tools like content_write
    // (per the existing Ri-0.6 tests). Clarification must not.
    assert!(
        degraded.allows("content_write"),
        "sanity: degraded mode allows content_write (Core tier)"
    );
    let clarif = determine_tool_tier_filter(&manifest, None, false, SessionState::Clarification, true);
    assert!(
        !clarif.allows("content_write"),
        "clarification mode must not allow content_write — read-only by construction"
    );
}
