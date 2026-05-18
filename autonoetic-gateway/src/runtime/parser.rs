//! SKILL.md Parser.

use autonoetic_types::agent::{
    AgentIO, AgentIdentity, AgentManifest, AgentSkillsImportMetadata, CompressionConfig,
    ExecutionMode, LlmConfig, Middleware, ResourceLimits, RuntimeDeclaration, ScriptInputMode,
};
use autonoetic_types::background::BackgroundPolicy;
use autonoetic_types::capability::Capability;
use gray_matter::{engine::YAML, Matter};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct StandardSkillFrontmatter {
    name: String,
    description: String,
    #[serde(default)]
    license: Option<String>,
    #[serde(default)]
    compatibility: Option<String>,
    #[serde(default, rename = "allowed-tools")]
    allowed_tools: Option<Vec<String>>,
    #[serde(default)]
    metadata: Option<StandardMetadataRoot>,
}

#[derive(Debug, Deserialize, Default)]
struct StandardMetadataRoot {
    #[serde(default)]
    autonoetic: Option<AutonoeticMetadata>,
}

#[derive(Debug, Deserialize, Default)]
struct AutonoeticMetadata {
    version: Option<String>,
    runtime: Option<RuntimeDeclaration>,
    agent: Option<AgentIdentity>,
    #[serde(default)]
    capabilities: Option<Vec<Capability>>,
    llm_config: Option<LlmConfig>,
    limits: Option<ResourceLimits>,
    background: Option<BackgroundPolicy>,
    #[serde(default)]
    disclosure: Option<autonoetic_types::disclosure::DisclosurePolicy>,
    #[serde(default)]
    io: Option<AgentIO>,
    #[serde(default)]
    middleware: Option<Middleware>,
    #[serde(default)]
    execution_mode: Option<ExecutionMode>,
    #[serde(default)]
    script_entry: Option<String>,
    #[serde(default)]
    script_input_mode: Option<ScriptInputMode>,
    #[serde(default)]
    gateway_url: Option<String>,
    #[serde(default)]
    gateway_token: Option<String>,
    #[serde(default)]
    allowed_tool_tiers: Option<Vec<autonoetic_types::agent::ToolTier>>,
    #[serde(default)]
    compression: Option<CompressionConfig>,
    #[serde(default)]
    sandbox_network: Option<autonoetic_types::agent::SandboxNetworkPolicy>,
}

/// Parser for `SKILL.md` files.
pub struct SkillParser;

impl SkillParser {
    /// Parses a `SKILL.md` content string into an `AgentManifest` and the Markdown body.
    pub fn parse(content: &str) -> anyhow::Result<(AgentManifest, String)> {
        let matter = Matter::<YAML>::new();
        let parsed = matter
            .parse(content)
            .map_err(|e| anyhow::anyhow!("gray_matter error: {}", e))?;

        let data: gray_matter::Pod = parsed
            .data
            .ok_or_else(|| anyhow::anyhow!("No YAML frontmatter found in SKILL.md"))?;
        reject_legacy_response_contract(content)?;

        let manifest = match data.deserialize::<AgentManifest>() {
            Ok(v) => v,
            Err(agent_manifest_err) => {
                let standard = data.deserialize::<StandardSkillFrontmatter>().map_err(|standard_err| {
                    anyhow::anyhow!(
                        "Invalid SKILL.md frontmatter. Autonoetic format error: {}. AgentSkills format error: {}",
                        agent_manifest_err,
                        standard_err
                    )
                })?;
                map_standard_frontmatter_to_manifest(standard)
            }
        };

        Ok((manifest, parsed.content))
    }
}

fn map_standard_frontmatter_to_manifest(standard: StandardSkillFrontmatter) -> AgentManifest {
    let meta = standard
        .metadata
        .and_then(|m| m.autonoetic)
        .unwrap_or_default();

    let runtime = meta.runtime.unwrap_or_else(default_runtime);
    let mut agent = meta.agent.unwrap_or_else(|| AgentIdentity {
        id: standard.name.clone(),
        name: standard.name.clone(),
        description: standard.description.clone(),
    });
    if agent.id.trim().is_empty() {
        agent.id = standard.name.clone();
    }
    if agent.name.trim().is_empty() {
        agent.name = standard.name.clone();
    }
    if agent.description.trim().is_empty() {
        agent.description = standard.description.clone();
    }

    let allowed_tools = standard.allowed_tools.unwrap_or_default();
    let has_agentskills_fields =
        standard.license.is_some() || standard.compatibility.is_some() || !allowed_tools.is_empty();

    let capabilities =
        if meta.capabilities.as_ref().map_or(true, |c| c.is_empty()) && !allowed_tools.is_empty() {
            infer_capabilities(&allowed_tools)
        } else {
            meta.capabilities.unwrap_or_default()
        };

    let agentskills_import = if has_agentskills_fields {
        Some(AgentSkillsImportMetadata {
            license: standard.license,
            compatibility: standard.compatibility,
            allowed_tools: allowed_tools.clone(),
            needs_tool_bridging: !allowed_tools.is_empty(),
        })
    } else {
        None
    };

    AgentManifest {
        version: meta.version.unwrap_or_else(|| "1.0".to_string()),
        runtime,
        agent,
        capabilities,
        llm_config: meta.llm_config,
        limits: meta.limits,
        background: meta.background,
        disclosure: meta.disclosure,
        io: meta.io,
        middleware: meta.middleware,
        execution_mode: meta.execution_mode.unwrap_or_default(),
        script_entry: meta.script_entry,
        script_input_mode: meta.script_input_mode.unwrap_or_default(),
        gateway_url: meta.gateway_url,
        gateway_token: meta.gateway_token,
        allowed_tool_tiers: meta.allowed_tool_tiers.unwrap_or_default(),
        agentskills_import,
        compression: meta.compression,
        sandbox_network: meta.sandbox_network.unwrap_or_default(),
    }
}

fn reject_legacy_response_contract(content: &str) -> anyhow::Result<()> {
    let mut parts = content.splitn(3, "---");
    let _ = parts.next();
    let Some(frontmatter) = parts.next() else {
        return Ok(());
    };

    let has_legacy_field = frontmatter
        .lines()
        .any(|line| line.trim_start().starts_with("response_contract:"));
    if has_legacy_field {
        anyhow::bail!(
            "response_contract is no longer supported; use metadata.autonoetic.io.output_policy and metadata.autonoetic.io.returns instead"
        );
    }
    Ok(())
}

/// Split instructions at the `<!-- extended -->` marker.
///
/// Everything before the marker is "core" (always injected in the system
/// prompt).  Everything after is "extended" (on-demand retrieval).  Returns
/// `(core, Some(extended))` if the marker is found, or `(body, None)` if not.
pub fn split_extended_instructions(body: &str) -> (&str, Option<&str>) {
    // Accept either <!-- extended --> or <!--extended--> (with/without spaces)
    for marker in &["<!-- extended -->", "<!--extended-->"] {
        if let Some(pos) = body.find(marker) {
            let core = body[..pos].trim();
            let extended = body[pos + marker.len()..].trim();
            let extended = if extended.is_empty() { None } else { Some(extended) };
            return (core, extended);
        }
    }
    (body, None)
}
///
/// Maps known AgentSkills tool names to Autonoetic capability types:
/// - `Bash(*)` → `SandboxFunctions` / `CodeExecution`
/// - `Read`/`View` → covered by baseline `ReadAccess`
/// - `Write`/`Edit` → `WriteAccess`
/// - `WebSearch`/`WebFetch` → `NetworkAccess`
pub fn infer_capabilities(allowed_tools: &[String]) -> Vec<Capability> {
    let mut caps = vec![Capability::ReadAccess {
        scopes: vec!["self.*".into()],
    }];

    let mut has_sandbox = false;
    let mut has_write = false;
    let mut has_network = false;
    let mut sandbox_patterns = Vec::new();

    for tool in allowed_tools {
        let t = tool.trim();
        if let Some(rest) = t.strip_prefix("Bash(").and_then(|s| s.strip_suffix(')')) {
            has_sandbox = true;
            if !rest.is_empty() && rest != "*" {
                for pattern in rest.split('|').map(|s| s.trim().to_string()) {
                    if !pattern.is_empty() && !sandbox_patterns.contains(&pattern) {
                        sandbox_patterns.push(pattern);
                    }
                }
            }
        } else if matches!(t, "Read" | "View") {
            // Already covered by baseline ReadAccess
        } else if matches!(t, "Write" | "Edit") {
            has_write = true;
        } else if matches!(t, "WebSearch" | "WebFetch" | "Fetch") {
            has_network = true;
        } else {
            if !sandbox_patterns.contains(&tool.clone()) {
                sandbox_patterns.push(tool.clone());
            }
            has_sandbox = true;
        }
    }

    if has_sandbox {
        if sandbox_patterns.is_empty() {
            sandbox_patterns.push("*".to_string());
        }
        caps.push(Capability::SandboxFunctions {
            allowed: sandbox_patterns,
        });
        caps.push(Capability::CodeExecution {
            patterns: vec!["*".to_string()],
            commands: vec![],
        });
    }

    if has_write {
        caps.push(Capability::WriteAccess {
            scopes: vec!["self.*".into(), "skills/*".into()],
        });
    }

    if has_network {
        caps.push(Capability::NetworkAccess {
            hosts: vec!["*".to_string()],
        });
    }

    caps
}

fn default_runtime() -> RuntimeDeclaration {
    RuntimeDeclaration {
        engine: "autonoetic".to_string(),
        gateway_version: "0.1.0".to_string(),
        sdk_version: "0.1.0".to_string(),
        runtime_type: "stateful".to_string(),
        sandbox: "bubblewrap".to_string(),
        runtime_lock: "runtime.lock".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_valid_skill() {
        let content = r#"---
version: "1.0"
runtime:
  engine: "autonoetic"
  gateway_version: "0.1.0"
  sdk_version: "0.1.0"
  type: "stateful"
  sandbox: "bubblewrap"
  runtime_lock: "uv.lock"
agent:
  id: "test_agent"
  name: "Test Agent"
  description: "A test agent"
---
# Test Agent Instructions
Here are the instructions.
"#;
        let (manifest, body) = SkillParser::parse(content).unwrap();
        assert_eq!(manifest.version, "1.0");
        assert_eq!(manifest.agent.id, "test_agent");
        assert_eq!(
            body.trim(),
            "# Test Agent Instructions\nHere are the instructions."
        );
    }

    #[test]
    fn test_parse_missing_frontmatter() {
        let content = "# Just markdown\nNo frontmatter here.";
        assert!(SkillParser::parse(content).is_err());
    }

    #[test]
    fn test_parse_agentskills_standard_with_autonoetic_metadata() {
        let content = r#"---
name: "test-agent"
description: "A standard AgentSkills entry"
metadata:
  autonoetic:
    version: "1.0"
    runtime:
      engine: "autonoetic"
      gateway_version: "0.1.0"
      sdk_version: "0.1.0"
      type: "stateful"
      sandbox: "bubblewrap"
      runtime_lock: "runtime.lock"
    agent:
      id: "test-agent"
      name: "Test Agent"
      description: "A standard AgentSkills entry"
    llm_config:
      provider: "openai"
      model: "gpt-4o"
      temperature: 0.2
---
# Test Agent Instructions
Use the skill.
"#;
        let (manifest, body) = SkillParser::parse(content).expect("should parse");
        assert_eq!(manifest.version, "1.0");
        assert_eq!(manifest.agent.id, "test-agent");
        assert_eq!(
            manifest.llm_config.as_ref().map(|c| c.provider.as_str()),
            Some("openai")
        );
        assert_eq!(body.trim(), "# Test Agent Instructions\nUse the skill.");
    }

    #[test]
    fn test_parse_background_policy() {
        let content = r#"---
version: "1.0"
runtime:
  engine: "autonoetic"
  gateway_version: "0.1.0"
  sdk_version: "0.1.0"
  type: "stateful"
  sandbox: "bubblewrap"
  runtime_lock: "runtime.lock"
agent:
  id: "bg-agent"
  name: "Background Agent"
  description: "Agent with background policy"
capabilities:
  - type: BackgroundReevaluation
    min_interval_secs: 30
    allow_reasoning: false
background:
  enabled: true
  interval_secs: 45
  mode: deterministic
  wake_predicates:
    timer: true
    approval_resolved: true
---
# Background Agent
"#;
        let (manifest, _body) = SkillParser::parse(content).expect("should parse");
        let background = manifest.background.expect("background should parse");
        assert!(background.enabled);
        assert_eq!(background.interval_secs, 45);
        assert!(background.wake_predicates.timer);
        assert!(background.wake_predicates.approval_resolved);
    }

    #[test]
    fn test_parse_io_schemas() {
        let content = r#"---
version: "1.0"
runtime:
  engine: "autonoetic"
  gateway_version: "0.1.0"
  sdk_version: "0.1.0"
  type: "stateful"
  sandbox: "bubblewrap"
  runtime_lock: "runtime.lock"
agent:
  id: "researcher"
  name: "Researcher"
  description: "A researcher"
io:
  accepts:
    type: object
    required:
      - query
    properties:
      query:
        type: string
      domain:
        type: string
  returns:
    type: object
    required:
      - findings
    properties:
      findings:
        type: array
      summary:
        type: string
---
# Researcher
"#;
        let (manifest, _body) = SkillParser::parse(content).expect("should parse");
        let io = manifest.io.expect("io should parse");
        let accepts = io.accepts.expect("accepts should exist");
        assert_eq!(accepts["type"], "object");
        let returns = io.returns.expect("returns should exist");
        assert_eq!(returns["type"], "object");
    }

    #[test]
    fn test_parse_without_io_schemas() {
        let content = r#"---
version: "1.0"
runtime:
  engine: "autonoetic"
  gateway_version: "0.1.0"
  sdk_version: "0.1.0"
  type: "stateful"
  sandbox: "bubblewrap"
  runtime_lock: "runtime.lock"
agent:
  id: "test"
  name: "Test"
  description: "A test"
---
# Test
"#;
        let (manifest, _body) = SkillParser::parse(content).expect("should parse");
        assert!(manifest.io.is_none());
    }

    #[test]
    fn test_parse_middleware_hooks() {
        let content = r#"---
version: "1.0"
runtime:
  engine: "autonoetic"
  gateway_version: "0.1.0"
  sdk_version: "0.1.0"
  type: "stateful"
  sandbox: "bubblewrap"
  runtime_lock: "runtime.lock"
agent:
  id: "test"
  name: "Test"
  description: "A test"
middleware:
  pre_process: "python3 scripts/pre.py"
  post_process: "python3 scripts/post.py"
---
# Test
"#;
        let (manifest, _body) = SkillParser::parse(content).expect("should parse");
        let middleware = manifest.middleware.expect("middleware should parse");
        assert_eq!(
            middleware.pre_process.as_deref(),
            Some("python3 scripts/pre.py")
        );
        assert_eq!(
            middleware.post_process.as_deref(),
            Some("python3 scripts/post.py")
        );
    }

    #[test]
    fn test_parse_execution_mode_script() {
        use autonoetic_types::agent::ExecutionMode;

        let content = r#"---
version: "1.0"
runtime:
  engine: "autonoetic"
  gateway_version: "0.1.0"
  sdk_version: "0.1.0"
  type: "stateful"
  sandbox: "bubblewrap"
  runtime_lock: "runtime.lock"
agent:
  id: "weather-script"
  name: "Weather Script"
  description: "A deterministic weather agent"
execution_mode: script
script_entry: scripts/weather.py
---
# Weather Script Agent
"#;
        let (manifest, _body) = SkillParser::parse(content).expect("should parse");
        assert_eq!(manifest.execution_mode, ExecutionMode::Script);
        assert_eq!(manifest.script_entry.as_deref(), Some("scripts/weather.py"));
    }

    #[test]
    fn test_parse_execution_mode_reasoning_default() {
        use autonoetic_types::agent::ExecutionMode;

        let content = r#"---
version: "1.0"
runtime:
  engine: "autonoetic"
  gateway_version: "0.1.0"
  sdk_version: "0.1.0"
  type: "stateful"
  sandbox: "bubblewrap"
  runtime_lock: "runtime.lock"
agent:
  id: "reasoning-agent"
  name: "Reasoning Agent"
  description: "A reasoning agent"
---
# Reasoning Agent
"#;
        let (manifest, _body) = SkillParser::parse(content).expect("should parse");
        assert_eq!(manifest.execution_mode, ExecutionMode::Reasoning);
        assert!(manifest.script_entry.is_none());
    }

    #[test]
    fn test_agentskills_allowed_tools_inference() {
        let content = r#"---
name: "git-helper"
description: "A git helper skill"
license: "MIT"
compatibility: "claude-code"
allowed-tools:
  - "Bash(git:*)"
  - "Read"
  - "Write"
  - "WebSearch"
---
# Git Helper
Use Bash(git log) to inspect history.
"#;
        let (manifest, _body) = SkillParser::parse(content).expect("should parse");
        assert_eq!(manifest.agent.id, "git-helper");

        let import = manifest
            .agentskills_import
            .expect("should have agentskills_import");
        assert_eq!(import.license.as_deref(), Some("MIT"));
        assert_eq!(import.compatibility.as_deref(), Some("claude-code"));
        assert_eq!(import.allowed_tools.len(), 4);
        assert!(import.needs_tool_bridging);

        let caps = &manifest.capabilities;
        assert!(caps
            .iter()
            .any(|c| matches!(c, Capability::ReadAccess { .. })));
        assert!(caps
            .iter()
            .any(|c| matches!(c, Capability::WriteAccess { .. })));
        assert!(caps
            .iter()
            .any(|c| matches!(c, Capability::NetworkAccess { .. })));
        assert!(caps
            .iter()
            .any(|c| matches!(c, Capability::SandboxFunctions { .. })));
    }

    #[test]
    fn test_agentskills_no_allowed_tools_no_import_metadata() {
        let content = r#"---
name: "simple-agent"
description: "No allowed tools"
metadata:
  autonoetic:
    version: "1.0"
    runtime:
      engine: "autonoetic"
      gateway_version: "0.1.0"
      sdk_version: "0.1.0"
      type: "stateful"
      sandbox: "bubblewrap"
      runtime_lock: "runtime.lock"
    agent:
      id: "simple-agent"
      name: "Simple Agent"
      description: "No allowed tools"
---
# Simple Agent
"#;
        let (manifest, _body) = SkillParser::parse(content).expect("should parse");
        assert!(
            manifest.agentskills_import.is_none(),
            "should not set agentskills_import without allowed-tools"
        );
    }

    #[test]
    fn test_agentskills_existing_capabilities_not_overridden() {
        let content = r#"---
name: "explicit-caps"
description: "Has explicit capabilities"
allowed-tools:
  - "Bash(*)"
metadata:
  autonoetic:
    capabilities:
      - type: ReadAccess
        scopes: ["global/*"]
---
# Explicit Caps
"#;
        let (manifest, _body) = SkillParser::parse(content).expect("should parse");
        assert_eq!(manifest.capabilities.len(), 1);
        assert!(
            manifest.capabilities.iter().any(|c| matches!(
                c,
                Capability::ReadAccess { scopes } if scopes.contains(&"global/*".to_string())
            )),
            "explicit capabilities should not be overridden by allowed-tools inference"
        );
    }

    #[test]
    fn test_infer_capabilities_bash_patterns() {
        let caps = infer_capabilities(&["Bash(git:*)".to_string(), "Bash(cargo:*)".to_string()]);
        let sandbox = caps
            .iter()
            .filter_map(|c| match c {
                Capability::SandboxFunctions { allowed } => Some(allowed),
                _ => None,
            })
            .next()
            .unwrap();
        assert!(sandbox.contains(&"git:*".to_string()));
        assert!(sandbox.contains(&"cargo:*".to_string()));
    }

    #[test]
    fn test_infer_capabilities_wildcard_bash() {
        let caps = infer_capabilities(&["Bash(*)".to_string(), "Read".to_string()]);
        let sandbox = caps
            .iter()
            .filter_map(|c| match c {
                Capability::SandboxFunctions { allowed } => Some(allowed),
                _ => None,
            })
            .next()
            .unwrap();
        assert_eq!(sandbox.len(), 1);
        assert_eq!(sandbox[0], "*");
    }

    #[test]
    fn test_split_extended_no_marker() {
        let body = "Just core instructions here.";
        let (core, extended) = split_extended_instructions(body);
        assert_eq!(core, body);
        assert!(extended.is_none());
    }

    #[test]
    fn test_split_extended_marker_with_spaces() {
        let body = "Core instructions.\n<!-- extended -->\nExtended instructions here.";
        let (core, extended) = split_extended_instructions(body);
        assert_eq!(core, "Core instructions.");
        assert_eq!(extended, Some("Extended instructions here."));
    }

    #[test]
    fn test_split_extended_marker_no_spaces() {
        let body = "Core instructions.\n<!--extended-->\nExtended instructions here.";
        let (core, extended) = split_extended_instructions(body);
        assert_eq!(core, "Core instructions.");
        assert_eq!(extended, Some("Extended instructions here."));
    }

    #[test]
    fn test_split_extended_marker_at_start() {
        let body = "<!-- extended -->\nAll content is extended.";
        let (core, extended) = split_extended_instructions(body);
        assert_eq!(core, "");
        assert_eq!(extended, Some("All content is extended."));
    }

    #[test]
    fn test_split_extended_empty_extended() {
        let body = "Core.\n<!-- extended -->\n   ";
        let (core, extended) = split_extended_instructions(body);
        assert_eq!(core, "Core.");
        assert!(extended.is_none());
    }

    #[test]
    fn test_split_extended_marker_at_end() {
        let body = "Core.\n<!-- extended -->";
        let (core, extended) = split_extended_instructions(body);
        assert_eq!(core, "Core.");
        assert!(extended.is_none());
    }
}
