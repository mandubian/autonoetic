//! Constitution Phase 4.7 pin: tool tiers come from a declarative registry.

use autonoetic_gateway::runtime::prompt_budget::tool_tier;
use autonoetic_gateway::runtime::tool_tier_registry::{
    load_registry_from_path, repository_default_registry_path, reset_registry_to_default,
};
use autonoetic_gateway::runtime::tools::ToolTierFilter;
use autonoetic_types::agent::ToolTier;
use serial_test::serial;
use tempfile::tempdir;

struct RegistryResetGuard;

impl Drop for RegistryResetGuard {
    fn drop(&mut self) {
        let _ = reset_registry_to_default();
    }
}

#[test]
#[serial]
fn default_registry_file_exists_and_is_applied() -> anyhow::Result<()> {
    let _guard = RegistryResetGuard;
    let registry_path = repository_default_registry_path();
    assert!(
        registry_path.exists(),
        "expected declarative registry at {}",
        registry_path.display()
    );
    load_registry_from_path(&registry_path)?;

    assert_eq!(tool_tier("content_write"), ToolTier::Core);
    assert_eq!(tool_tier("approval_status"), ToolTier::Workflow);
    assert_eq!(tool_tier("web_search"), ToolTier::Specialized);
    Ok(())
}

#[test]
#[serial]
fn tool_tier_filter_uses_registry_rules_not_hardcoded_constants() -> anyhow::Result<()> {
    let _guard = RegistryResetGuard;
    let dir = tempdir()?;
    let registry_path = dir.path().join("tools.yaml");
    std::fs::write(
        &registry_path,
        r#"version: 1
default_tier: specialized
rules:
  - prefix: web_
    tier: core
  - prefix: content_
    tier: specialized
"#,
    )?;
    load_registry_from_path(&registry_path)?;

    assert_eq!(tool_tier("web_search"), ToolTier::Core);
    assert_eq!(tool_tier("content_write"), ToolTier::Specialized);

    let core_filter = ToolTierFilter::core_only();
    assert!(core_filter.allows("web_search"));
    assert!(!core_filter.allows("content_write"));
    Ok(())
}
