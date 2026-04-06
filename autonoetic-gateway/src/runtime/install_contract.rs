//! Install Contract Helpers — canonical defaults, scaffolding, and diagnostics.
//!
//! This module is the single source of truth for:
//! - Gateway-owned canonical defaults (runtime engine, versions, sandbox)
//! - Runtime lock scaffolding (gateway/sdk/sandbox fields filled deterministically)
//! - Install validation helpers (shape checking before typed deserialization)
//! - Diagnostic examples (canonical SKILL.md and runtime.lock examples)
//!
//! Design rule: This is deterministic infrastructure, not a policy engine.
//! No inference from code meaning, no guessing agent intent.

use autonoetic_types::agent::RuntimeDeclaration;
use autonoetic_types::layer::ArtifactLayer;
use autonoetic_types::runtime_lock::{
    LockedArtifact, LockedDependencySet, LockedGateway, LockedLayerMount, LockedSandbox, LockedSdk,
    RuntimeLock,
};

// ─── Canonical defaults ────────────────────────────────────────

pub const DEFAULT_ENGINE: &str = "autonoetic";
pub const DEFAULT_RUNTIME_TYPE: &str = "stateful";
pub const DEFAULT_SANDBOX: &str = "bubblewrap";
pub const DEFAULT_RUNTIME_LOCK_FILENAME: &str = "runtime.lock";
pub const PLACEHOLDER_SHA: &str = "replace-me";

pub fn gateway_version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}

pub fn sdk_version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}

pub fn default_gateway_artifact() -> String {
    "marketplace://gateway/autonoetic-gateway".to_string()
}

// ─── RuntimeDeclaration helpers ────────────────────────────────

pub fn default_runtime_declaration() -> RuntimeDeclaration {
    RuntimeDeclaration {
        engine: DEFAULT_ENGINE.to_string(),
        gateway_version: gateway_version(),
        sdk_version: sdk_version(),
        runtime_type: DEFAULT_RUNTIME_TYPE.to_string(),
        sandbox: DEFAULT_SANDBOX.to_string(),
        runtime_lock: DEFAULT_RUNTIME_LOCK_FILENAME.to_string(),
    }
}

// ─── RuntimeLock scaffolding ───────────────────────────────────

pub fn scaffold_runtime_lock(
    agent_dependencies: Option<Vec<LockedDependencySet>>,
    agent_artifacts: Option<Vec<LockedArtifact>>,
    artifact_layers: &[ArtifactLayer],
) -> RuntimeLock {
    RuntimeLock {
        gateway: LockedGateway {
            artifact: default_gateway_artifact(),
            version: gateway_version(),
            sha256: PLACEHOLDER_SHA.to_string(),
            signature: None,
        },
        sdk: LockedSdk {
            version: sdk_version(),
        },
        sandbox: LockedSandbox {
            backend: DEFAULT_SANDBOX.to_string(),
        },
        dependencies: agent_dependencies.unwrap_or_default(),
        artifacts: agent_artifacts.unwrap_or_default(),
        layers: artifact_layers
            .iter()
            .map(|l| LockedLayerMount {
                layer_id: l.layer_id.clone(),
                digest: l.digest.clone(),
                mount_path: l.mount_path.clone(),
            })
            .collect(),
    }
}

pub fn default_runtime_lock(artifact_layers: &[ArtifactLayer]) -> RuntimeLock {
    scaffold_runtime_lock(None, None, artifact_layers)
}

// ─── Example rendering for diagnostics ─────────────────────────

pub fn render_skill_metadata_example() -> String {
    r#"---
version: "1.0"
runtime:
  engine: "autonoetic"
  gateway_version: "0.1.0"
  sdk_version: "0.1.0"
  type: "stateful"
  sandbox: "bubblewrap"
  runtime_lock: "runtime.lock"
agent:
  id: "my.agent"
  name: "My Agent"
  description: "What this agent does"
llm_config:
  provider: "openai"
  model: "gpt-4o"
capabilities:
  - type: "ReadAccess"
    scopes: ["*"]
  - type: "WriteAccess"
    scopes: ["self.*"]
execution_mode: "reasoning"
---
# Your agent instructions go here in markdown.
"#
    .to_string()
}

pub fn render_runtime_lock_example() -> String {
    r#"gateway:
  artifact: "marketplace://gateway/autonoetic-gateway"
  version: "0.1.0"
  sha256: "replace-me"
sdk:
  version: "0.1.0"
sandbox:
  backend: "bubblewrap"
dependencies: []
artifacts: []
layers: []
"#
    .to_string()
}

// ─── Schema description for agent.revision.schema tool ─────────

pub fn install_schema_description() -> String {
    r#"# Agent Install Contract

## Ownership Split

**Agent-owned (free-form):**
- Markdown body of SKILL.md (instructions, role, workflow notes)

**Agent-provided (semantic intent):**
- agent.id, description, execution_mode, script_entry, llm_config, capabilities
- Optional: io, middleware, response_contract

**Gateway-owned (canonicalized):**
- SKILL.md metadata shape and field types
- runtime.engine, runtime.gateway_version, runtime.sdk_version
- Final canonical SKILL.md metadata serialization
- Final canonical runtime.lock serialization

**Runtime lock — Gateway-owned (autofilled):**
- gateway (artifact, version, sha256)
- sdk (version)
- sandbox (backend)
- artifacts, layers (from artifact)

**Runtime lock — Agent-provided intent:**
- dependencies (package hints)
- artifacts (optional agent-provided artifact references)

## Accepted SKILL.md Shapes

Two frontmatter shapes are accepted:

**Top-level Autonoetic shape:**
- `runtime` (object, required)
  - `engine`, `gateway_version`, `sdk_version`, `type`, `runtime_lock`
- `agent` (object, required)
  - `id` (required), `name`, `description`

**Metadata-wrapped shape (AgentSkills-compatible):**
- `name` (string, required)
- `description` (string, required)
- `metadata.autonoetic.runtime` (object, required)
  - `engine`, `gateway_version`, `sdk_version`, `type`, `runtime_lock`
- `metadata.autonoetic.agent` (object, required)
  - `id` (required), `name`, `description`

## runtime.lock — What You Need to Provide

**Agent-owned (optional):**
- `dependencies` (array) — package hints like `[{runtime: "python3", packages: ["pip"]}]`
- `artifacts` (array) — agent-provided artifact references

**Gateway-autofilled (do not hand-author):**
- `gateway`, `sdk`, `sandbox`, `layers` — filled automatically by the gateway during install
- If you provide a partial `runtime.lock`, missing gateway-owned sections are scaffolded

## Guidance
- Use `artifact_id` to pass file bundles to agent.revision.create
- The gateway fills gateway/sdk/sandbox/layers automatically
- You only need to provide agent identity and intent
- For a minimal agent bundle, a `runtime.lock` with `dependencies: []` and `artifacts: []` is sufficient
"#
    .to_string()
}

// ─── Validation helpers for agent.revision.create ──────────────

#[derive(Debug, Default, serde::Deserialize)]
pub struct RuntimeLockPartial {
    #[serde(default)]
    pub dependencies: Option<Vec<serde_yaml::Value>>,
    #[serde(default)]
    pub artifacts: Option<Vec<serde_yaml::Value>>,
}

pub fn validate_runtime_lock_shape(yaml: &serde_yaml::Value) -> Vec<String> {
    let mut missing = Vec::new();

    let obj = yaml.as_mapping();
    let Some(obj) = obj else {
        return vec!["runtime.lock must be a YAML mapping".to_string()];
    };

    let deps_key = serde_yaml::Value::String("dependencies".into());
    if let Some(deps) = obj.get(&deps_key) {
        if !deps.is_sequence() && !deps.is_null() {
            missing.push("dependencies (must be a sequence)".to_string());
        } else if let Some(deps_arr) = deps.as_sequence() {
            for (i, dep) in deps_arr.iter().enumerate() {
                if dep.as_mapping().is_none() {
                    missing.push(format!("dependencies[{}] must be a mapping", i));
                } else {
                    let dm = dep.as_mapping().unwrap();
                    let runtime_key = serde_yaml::Value::String("runtime".into());
                    if dm.get(&runtime_key).is_none() {
                        missing.push(format!("dependencies[{}].runtime", i));
                    }
                }
            }
        }
    }

    let arts_key = serde_yaml::Value::String("artifacts".into());
    if let Some(arts) = obj.get(&arts_key) {
        if !arts.is_sequence() && !arts.is_null() {
            missing.push("artifacts (must be a sequence)".to_string());
        } else if let Some(arts_arr) = arts.as_sequence() {
            for (i, art) in arts_arr.iter().enumerate() {
                if art.as_mapping().is_none() {
                    missing.push(format!("artifacts[{}] must be a mapping", i));
                }
            }
        }
    }

    missing
}

pub fn validate_skill_frontmatter_shape(frontmatter: &serde_yaml::Value) -> Vec<String> {
    let mut missing = Vec::new();

    let obj = frontmatter.as_mapping();
    let Some(obj) = obj else {
        return vec!["SKILL.md frontmatter must be a YAML mapping".to_string()];
    };

    let type_key = serde_yaml::Value::String("type".into());
    let runtime_lock_key = serde_yaml::Value::String("runtime_lock".into());
    let id_key = serde_yaml::Value::String("id".into());

    let (runtime_map, agent_map, used_metadata_path) = if obj
        .get(&serde_yaml::Value::String("runtime".into()))
        .is_some()
    {
        let rt = obj
            .get(&serde_yaml::Value::String("runtime".into()))
            .unwrap();
        let ag = obj.get(&serde_yaml::Value::String("agent".into()));
        (rt.as_mapping(), ag.and_then(|v| v.as_mapping()), false)
    } else if let Some(meta) = obj
        .get(&serde_yaml::Value::String("metadata".into()))
        .and_then(|v| v.as_mapping())
    {
        let autonoetic = meta
            .get(&serde_yaml::Value::String("autonoetic".into()))
            .and_then(|v| v.as_mapping());
        if let Some(auto) = autonoetic {
            (
                auto.get(&serde_yaml::Value::String("runtime".into()))
                    .and_then(|v| v.as_mapping()),
                auto.get(&serde_yaml::Value::String("agent".into()))
                    .and_then(|v| v.as_mapping()),
                true,
            )
        } else {
            missing.push("metadata.autonoetic".to_string());
            (None, None, true)
        }
    } else {
        missing.push("runtime (or metadata.autonoetic.runtime)".to_string());
        (None, None, false)
    };

    if used_metadata_path {
        let name_key = serde_yaml::Value::String("name".into());
        let desc_key = serde_yaml::Value::String("description".into());
        if obj.get(&name_key).is_none() {
            missing.push("name".to_string());
        }
        if obj.get(&desc_key).is_none() {
            missing.push("description".to_string());
        }
    }

    if let Some(rt) = runtime_map {
        let engine_key = serde_yaml::Value::String("engine".into());
        let gw_ver_key = serde_yaml::Value::String("gateway_version".into());
        let sdk_ver_key = serde_yaml::Value::String("sdk_version".into());
        if rt.get(&engine_key).is_none() {
            missing.push("runtime.engine".to_string());
        }
        if rt.get(&gw_ver_key).is_none() {
            missing.push("runtime.gateway_version".to_string());
        }
        if rt.get(&sdk_ver_key).is_none() {
            missing.push("runtime.sdk_version".to_string());
        }
        if rt.get(&type_key).is_none() {
            missing.push("runtime.type".to_string());
        }
        if rt.get(&runtime_lock_key).is_none() {
            missing.push("runtime.runtime_lock".to_string());
        }
    } else if !missing.iter().any(|m| m.contains("runtime")) {
        missing.push("runtime (must be a mapping)".to_string());
    }

    if let Some(ag) = agent_map {
        if ag.get(&id_key).is_none() {
            missing.push("agent.id".to_string());
        }
    } else if obj
        .get(&serde_yaml::Value::String("agent".into()))
        .is_some()
    {
        // agent key exists but is not a mapping
        missing.push("agent (must be a mapping)".to_string());
    } else if !missing.iter().any(|m| m == "metadata.autonoetic") {
        missing.push("agent (or metadata.autonoetic.agent)".to_string());
    }

    missing
}

pub fn extract_frontmatter_raw(content: &str) -> anyhow::Result<serde_yaml::Value> {
    use gray_matter::{engine::YAML, Matter};
    let matter = Matter::<YAML>::new();
    let parsed = matter
        .parse(content)
        .map_err(|e| anyhow::anyhow!("Failed to parse SKILL.md frontmatter: {}", e))?;

    let pod: gray_matter::Pod = parsed
        .data
        .ok_or_else(|| anyhow::anyhow!("No YAML frontmatter found in SKILL.md"))?;

    let value: serde_yaml::Value = pod
        .deserialize::<serde_yaml::Value>()
        .map_err(|e| anyhow::anyhow!("Failed to deserialize frontmatter: {}", e))?;

    Ok(value)
}

pub fn format_install_validation_error(
    skill_missing: &[String],
    lock_missing: Option<&[String]>,
    parse_error: Option<&str>,
) -> String {
    let mut msg = String::from("Install validation failed:\n");

    if !skill_missing.is_empty() {
        msg.push_str("\nSKILL.md issues:\n");
        for path in skill_missing {
            msg.push_str(&format!("  - Missing or invalid: `{}`\n", path));
        }
    }

    if let Some(lm) = lock_missing {
        if !lm.is_empty() {
            msg.push_str("\nruntime.lock issues:\n");
            for path in lm {
                msg.push_str(&format!("  - Missing or invalid: `{}`\n", path));
            }
        }
    }

    if let Some(err) = parse_error {
        msg.push_str(&format!("\nParse error: {}\n", err));
    }

    msg.push_str("\nCanonical examples:\n");
    msg.push_str(&format!(
        "\n--- SKILL.md example ---\n{}\n",
        render_skill_metadata_example()
    ));
    msg.push_str(&format!(
        "--- runtime.lock example ---\n{}\n",
        render_runtime_lock_example()
    ));
    msg.push_str("\nUse agent.revision.schema for full schema documentation.\n");

    msg
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_runtime_declaration() {
        let rt = default_runtime_declaration();
        assert_eq!(rt.engine, "autonoetic");
        assert_eq!(rt.runtime_type, "stateful");
        assert_eq!(rt.sandbox, "bubblewrap");
        assert_eq!(rt.runtime_lock, "runtime.lock");
    }

    #[test]
    fn test_scaffold_runtime_lock_no_layers() {
        let lock = scaffold_runtime_lock(None, None, &[]);
        assert_eq!(
            lock.gateway.artifact,
            "marketplace://gateway/autonoetic-gateway"
        );
        assert_eq!(lock.gateway.sha256, "replace-me");
        assert_eq!(lock.sandbox.backend, "bubblewrap");
        assert!(lock.dependencies.is_empty());
        assert!(lock.layers.is_empty());
    }

    #[test]
    fn test_scaffold_runtime_lock_with_layers() {
        let layers = vec![ArtifactLayer {
            layer_id: "layer_abc".into(),
            name: "python-deps".into(),
            mount_path: "/deps".into(),
            digest: "sha256:xyz".into(),
        }];
        let lock = scaffold_runtime_lock(None, None, &layers);
        assert_eq!(lock.layers.len(), 1);
        assert_eq!(lock.layers[0].layer_id, "layer_abc");
        assert_eq!(lock.layers[0].mount_path, "/deps");
    }

    #[test]
    fn test_validate_runtime_lock_shape_valid() {
        let yaml: serde_yaml::Value = serde_yaml::from_str(
            r#"
gateway:
  artifact: "marketplace://gw"
  version: "0.1.0"
  sha256: "abc"
sdk:
  version: "0.1.0"
sandbox:
  backend: "bubblewrap"
"#,
        )
        .unwrap();
        let missing = validate_runtime_lock_shape(&yaml);
        assert!(missing.is_empty());
    }

    #[test]
    fn test_validate_runtime_lock_shape_bad_dependency() {
        let yaml: serde_yaml::Value = serde_yaml::from_str(
            r#"
dependencies:
  - "not_a_mapping"
  - runtime: "python3"
    packages: ["pip"]
"#,
        )
        .unwrap();
        let missing = validate_runtime_lock_shape(&yaml);
        assert_eq!(missing, vec!["dependencies[0] must be a mapping"]);
    }

    #[test]
    fn test_validate_runtime_lock_shape_missing_dep_runtime() {
        let yaml: serde_yaml::Value = serde_yaml::from_str(
            r#"
dependencies:
  - packages: ["pip"]
"#,
        )
        .unwrap();
        let missing = validate_runtime_lock_shape(&yaml);
        assert_eq!(missing, vec!["dependencies[0].runtime"]);
    }

    #[test]
    fn test_validate_skill_frontmatter_shape_valid() {
        let yaml: serde_yaml::Value = serde_yaml::from_str(
            r#"
version: "1.0"
runtime:
  engine: "autonoetic"
  gateway_version: "0.1.0"
  sdk_version: "0.1.0"
  type: "stateful"
  runtime_lock: "runtime.lock"
agent:
  id: "my.agent"
  name: "My Agent"
  description: "Desc"
"#,
        )
        .unwrap();
        let missing = validate_skill_frontmatter_shape(&yaml);
        assert!(missing.is_empty());
    }

    #[test]
    fn test_validate_skill_frontmatter_shape_missing_agent() {
        let yaml: serde_yaml::Value = serde_yaml::from_str(
            r#"
version: "1.0"
runtime:
  engine: "autonoetic"
  gateway_version: "0.1.0"
  sdk_version: "0.1.0"
  type: "stateful"
  runtime_lock: "runtime.lock"
"#,
        )
        .unwrap();
        let missing = validate_skill_frontmatter_shape(&yaml);
        assert_eq!(missing, vec!["agent (or metadata.autonoetic.agent)"]);
    }

    #[test]
    fn test_validate_skill_frontmatter_shape_missing_agent_id() {
        let yaml: serde_yaml::Value = serde_yaml::from_str(
            r#"
version: "1.0"
runtime:
  engine: "autonoetic"
  gateway_version: "0.1.0"
  sdk_version: "0.1.0"
  type: "stateful"
  runtime_lock: "runtime.lock"
agent:
  name: "My Agent"
  description: "Desc"
"#,
        )
        .unwrap();
        let missing = validate_skill_frontmatter_shape(&yaml);
        assert_eq!(missing, vec!["agent.id"]);
    }

    #[test]
    fn test_extract_frontmatter_raw_valid() {
        let content = r#"---
name: "test.agent"
runtime:
  engine: "autonoetic"
---
# Body
"#;
        let value = extract_frontmatter_raw(content).unwrap();
        assert!(value.get("name").is_some());
        assert!(value.get("runtime").is_some());
    }

    #[test]
    fn test_extract_frontmatter_raw_no_frontmatter() {
        let content = "# Just markdown, no frontmatter";
        let result = extract_frontmatter_raw(content);
        assert!(result.is_err());
    }

    #[test]
    fn test_format_install_validation_error() {
        let skill_missing = vec!["agent.id".to_string()];
        let lock_missing = vec!["gateway.sha256".to_string()];
        let msg = format_install_validation_error(&skill_missing, Some(&lock_missing), None);
        assert!(msg.contains("SKILL.md issues"));
        assert!(msg.contains("agent.id"));
        assert!(msg.contains("runtime.lock issues"));
        assert!(msg.contains("gateway.sha256"));
        assert!(msg.contains("SKILL.md example"));
        assert!(msg.contains("runtime.lock example"));
    }

    #[test]
    fn test_validate_skill_frontmatter_shape_metadata_wrapped_missing_name() {
        let yaml: serde_yaml::Value = serde_yaml::from_str(
            r#"
description: "A skill"
metadata:
  autonoetic:
    runtime:
      engine: "autonoetic"
      gateway_version: "0.1.0"
      sdk_version: "0.1.0"
      type: "stateful"
      runtime_lock: "runtime.lock"
    agent:
      id: "my.agent"
      name: "My Agent"
      description: "Desc"
"#,
        )
        .unwrap();
        let missing = validate_skill_frontmatter_shape(&yaml);
        assert!(missing.contains(&"name".to_string()));
    }

    #[test]
    fn test_validate_skill_frontmatter_shape_metadata_wrapped_missing_description() {
        let yaml: serde_yaml::Value = serde_yaml::from_str(
            r#"
name: "my.agent"
metadata:
  autonoetic:
    runtime:
      engine: "autonoetic"
      gateway_version: "0.1.0"
      sdk_version: "0.1.0"
      type: "stateful"
      runtime_lock: "runtime.lock"
    agent:
      id: "my.agent"
      name: "My Agent"
      description: "Desc"
"#,
        )
        .unwrap();
        let missing = validate_skill_frontmatter_shape(&yaml);
        assert!(missing.contains(&"description".to_string()));
    }

    #[test]
    fn test_validate_skill_frontmatter_shape_metadata_wrapped_valid() {
        let yaml: serde_yaml::Value = serde_yaml::from_str(
            r#"
name: "my.agent"
description: "A skill"
metadata:
  autonoetic:
    runtime:
      engine: "autonoetic"
      gateway_version: "0.1.0"
      sdk_version: "0.1.0"
      type: "stateful"
      runtime_lock: "runtime.lock"
    agent:
      id: "my.agent"
      name: "My Agent"
      description: "Desc"
"#,
        )
        .unwrap();
        let missing = validate_skill_frontmatter_shape(&yaml);
        assert!(missing.is_empty());
    }

    #[test]
    fn test_validate_runtime_lock_shape_dependencies_wrong_type() {
        let yaml: serde_yaml::Value = serde_yaml::from_str(
            r#"
dependencies: "not_a_sequence"
"#,
        )
        .unwrap();
        let missing = validate_runtime_lock_shape(&yaml);
        assert!(missing.contains(&"dependencies (must be a sequence)".to_string()));
    }

    #[test]
    fn test_validate_runtime_lock_shape_artifacts_wrong_type() {
        let yaml: serde_yaml::Value = serde_yaml::from_str(
            r#"
artifacts: "not_a_sequence"
"#,
        )
        .unwrap();
        let missing = validate_runtime_lock_shape(&yaml);
        assert!(missing.contains(&"artifacts (must be a sequence)".to_string()));
    }

    #[test]
    fn test_schema_description_mentions_both_shapes() {
        let desc = install_schema_description();
        assert!(desc.contains("Top-level Autonoetic shape"));
        assert!(desc.contains("Metadata-wrapped shape"));
        assert!(desc.contains("Gateway-autofilled"));
        assert!(desc.contains("dependencies"));
        assert!(!desc.contains("`gateway` (object, required)"));
    }
}
