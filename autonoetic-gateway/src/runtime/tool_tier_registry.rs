use autonoetic_types::agent::ToolTier;
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::sync::{OnceLock, RwLock};

const REGISTRY_ENV: &str = "AUTONOETIC_TOOL_TIER_REGISTRY_PATH";
const DEFAULT_REGISTRY_PATH: &str = "config/tools.yaml";
const EMBEDDED_DEFAULT_REGISTRY: &str = include_str!("../../../config/tools.yaml");

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct ToolTierRegistryFile {
    #[serde(default = "default_registry_version")]
    version: u32,
    default_tier: ToolTier,
    #[serde(default)]
    rules: Vec<ToolTierRule>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct ToolTierRule {
    prefix: String,
    #[serde(default = "default_rule_tier")]
    tier: ToolTier,
}

fn default_rule_tier() -> ToolTier {
    ToolTier::Core
}

#[derive(Debug, Clone)]
struct ToolTierRegistry {
    default_tier: ToolTier,
    rules: Vec<ToolTierRule>,
}

impl ToolTierRegistry {
    fn from_yaml(yaml: &str) -> anyhow::Result<Self> {
        let parsed: ToolTierRegistryFile = serde_yaml::from_str(yaml)
            .map_err(|e| anyhow::anyhow!("failed to parse tool-tier registry yaml: {e}"))?;
        anyhow::ensure!(
            parsed.version == 1,
            "unsupported tool-tier registry version {}; expected 1",
            parsed.version
        );
        for (idx, rule) in parsed.rules.iter().enumerate() {
            anyhow::ensure!(
                !rule.prefix.trim().is_empty(),
                "tool-tier rule #{idx} has empty prefix"
            );
        }
        Ok(Self {
            default_tier: parsed.default_tier,
            rules: parsed.rules,
        })
    }

    fn tier_for_tool(&self, tool_name: &str) -> ToolTier {
        for rule in &self.rules {
            if tool_name.starts_with(&rule.prefix) {
                return rule.tier;
            }
        }
        self.default_tier
    }
}

fn default_registry_version() -> u32 {
    1
}

fn default_registry() -> ToolTierRegistry {
    ToolTierRegistry::from_yaml(EMBEDDED_DEFAULT_REGISTRY).unwrap_or_else(|e| {
        tracing::error!(
            target: "autonoetic::tool_tier_registry",
            error = %e,
            "Embedded default tool-tier registry is invalid; falling back to specialized-only mapping"
        );
        ToolTierRegistry {
            default_tier: ToolTier::Specialized,
            rules: Vec::new(),
        }
    })
}

static TOOL_TIER_REGISTRY: OnceLock<RwLock<ToolTierRegistry>> = OnceLock::new();

fn registry_lock() -> &'static RwLock<ToolTierRegistry> {
    TOOL_TIER_REGISTRY.get_or_init(|| RwLock::new(default_registry()))
}

pub fn registry_path_from_env() -> PathBuf {
    std::env::var(REGISTRY_ENV)
        .ok()
        .filter(|v| !v.trim().is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_REGISTRY_PATH))
}

pub fn repository_default_registry_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../config/tools.yaml")
        .canonicalize()
        .unwrap_or_else(|_| Path::new(env!("CARGO_MANIFEST_DIR")).join("../config/tools.yaml"))
}

/// Initialize (or refresh) the tool-tier registry from startup config path.
///
/// If the configured file does not exist, the embedded default registry is used.
pub fn initialize_from_startup_path() -> anyhow::Result<()> {
    if std::env::var(REGISTRY_ENV)
        .ok()
        .filter(|v| !v.trim().is_empty())
        .is_some()
    {
        let path = registry_path_from_env();
        if !path.exists() {
            anyhow::bail!(
                "configured tool-tier registry path does not exist: {}",
                path.display()
            );
        }
        return load_registry_from_path(&path);
    }

    let primary = PathBuf::from(DEFAULT_REGISTRY_PATH);
    if primary.exists() {
        return load_registry_from_path(&primary);
    }
    let repo_relative = repository_default_registry_path();
    if repo_relative.exists() {
        return load_registry_from_path(&repo_relative);
    }
    reset_registry_to_default()
}

pub fn load_registry_from_path(path: &Path) -> anyhow::Result<()> {
    let content = std::fs::read_to_string(path).map_err(|e| {
        anyhow::anyhow!(
            "failed to read tool-tier registry from '{}': {e}",
            path.display()
        )
    })?;
    let parsed = ToolTierRegistry::from_yaml(&content)?;
    let mut guard = registry_lock()
        .write()
        .map_err(|e| anyhow::anyhow!("tool-tier registry lock poisoned: {e}"))?;
    *guard = parsed;
    tracing::info!(
        target: "autonoetic::tool_tier_registry",
        path = %path.display(),
        "Loaded tool-tier registry"
    );
    Ok(())
}

pub fn reset_registry_to_default() -> anyhow::Result<()> {
    let mut guard = registry_lock()
        .write()
        .map_err(|e| anyhow::anyhow!("tool-tier registry lock poisoned: {e}"))?;
    *guard = default_registry();
    Ok(())
}

pub fn tool_tier(tool_name: &str) -> ToolTier {
    registry_lock()
        .read()
        .map(|registry| registry.tier_for_tool(tool_name))
        .unwrap_or(ToolTier::Specialized)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_registry_assigns_expected_tiers() {
        let r = default_registry();
        // self_describe is a right-surfacing self-knowledge tool — must be Core
        // so child sessions and budget-pressured turns still see it (#315).
        assert_eq!(
            r.tier_for_tool("self_describe"),
            ToolTier::Core,
            "self_describe must be Core"
        );
        // Anchors so the registry shape can't silently regress.
        assert_eq!(r.tier_for_tool("knowledge_search"), ToolTier::Core);
        assert_eq!(r.tier_for_tool("artifact_inspect"), ToolTier::Core);
        assert_eq!(r.tier_for_tool("agent_spawn"), ToolTier::Workflow);
        assert_eq!(r.tier_for_tool("federation.escalate"), ToolTier::Workflow);
        assert_eq!(r.tier_for_tool("promotion_query"), ToolTier::Workflow);
        assert_eq!(r.tier_for_tool("promotion_record"), ToolTier::Specialized);
        assert_eq!(r.tier_for_tool("credential_setup"), ToolTier::Workflow);
        assert_eq!(r.tier_for_tool("skill_normalize"), ToolTier::Workflow);
        assert_eq!(r.tier_for_tool("scheduler_cron_create"), ToolTier::Workflow);
        assert_eq!(r.tier_for_tool("web_search"), ToolTier::Specialized);
        assert_eq!(
            r.tier_for_tool("totally_unknown_tool"),
            ToolTier::Specialized,
            "unknown tools fall to the default tier"
        );
    }
}
