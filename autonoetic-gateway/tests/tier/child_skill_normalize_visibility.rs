//! `skill_normalize` visibility for skills/-writer child sessions.
//!
//! `skill_normalize` is Core-tier with a capability gate: `is_available`
//! requires `WriteAccess` covering `skills/` (same boundary shape as
//! `sandbox_exec` being Core while `CodeExecution` gates it). The point of
//! this suite is the *child-session* case: `child_tool_tier_filter_for_manifest`
//! yields exactly `ToolTierFilter::core_only()` for a manifest that holds no
//! Workflow-granting capability (researcher/packager shape), so before the
//! tier change those children could hold the capability yet never see the
//! tool — and skill extraction degraded into hand-rolled markdown parsing.
//!
//! These tests pin the contract through the public registry API, reproducing
//! the child filter shape with `ToolTierFilter::core_only()`.

use autonoetic_gateway::runtime::tools::{default_registry, ToolTierFilter};
use autonoetic_gateway::runtime::tool_tier_registry::tool_tier;
use autonoetic_types::agent::AgentManifest;
use autonoetic_types::capability::Capability;
use crate::support::manifest_builder::TestManifest;

fn researcher_like_manifest() -> AgentManifest {
    AgentManifest {
        capabilities: vec![Capability::WriteAccess {
            scopes: vec!["self.*".to_string(), "skills/*".to_string()],
        }],
        ..TestManifest::new().build()
    }
}

fn executor_like_manifest() -> AgentManifest {
    AgentManifest {
        capabilities: vec![Capability::WriteAccess {
            scopes: vec!["self.*".to_string()],
        }],
        ..TestManifest::new().build()
    }
}

fn filtered_names(manifest: &AgentManifest) -> Vec<String> {
    let filter = ToolTierFilter::core_only();
    default_registry()
        .available_definitions_filtered(manifest, Some(&filter))
        .iter()
        .map(|d| d.name.clone())
        .collect()
}

#[test]
fn skill_normalize_is_core_tier() {
    assert_eq!(
        tool_tier("skill_normalize"),
        autonoetic_types::agent::ToolTier::Core,
        "skill_normalize must stay Core: the skills/ WriteAccess gate is the boundary"
    );
}

#[test]
fn skills_writer_child_sees_skill_normalize() {
    let names = filtered_names(&researcher_like_manifest());
    assert!(
        names.contains(&"skill_normalize".to_string()),
        "child session with skills/ WriteAccess must see skill_normalize; got {names:?}"
    );
}

#[test]
fn capability_gate_still_hides_skill_normalize_without_skills_write() {
    let names = filtered_names(&executor_like_manifest());
    assert!(
        !names.contains(&"skill_normalize".to_string()),
        "no skills/ WriteAccess ⇒ no skill_normalize, regardless of tier"
    );
}

#[test]
fn manifest_exclusion_still_hides_skill_normalize_for_planner() {
    let mut manifest = researcher_like_manifest();
    manifest.excluded_tools = vec!["skill_normalize".to_string()];
    let names = filtered_names(&manifest);
    assert!(
        !names.contains(&"skill_normalize".to_string()),
        "planner-style excluded_tools must keep skill_normalize hidden even with the capability"
    );
}
