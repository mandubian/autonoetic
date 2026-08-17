//! SKILL.md Parser.

use autonoetic_types::agent::{
    AgentEgressManifest, AgentIO, AgentIdentity, AgentManifest, AgentSkillsImportMetadata, CompressionConfig,
    ExecutionMode, IoReturnsEnforcement, LlmConfig, Middleware, ResourceLimits, RuntimeDeclaration,
    ScriptInputMode,
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
    #[serde(default)]
    llm_preset: Option<String>,
    #[serde(default)]
    llm_overrides: Option<autonoetic_types::agent::LlmOverrides>,
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
    excluded_tools: Option<Vec<String>>,
    #[serde(default)]
    sections: Option<Vec<autonoetic_types::agent::SectionGate>>,
    #[serde(default)]
    compression: Option<CompressionConfig>,
    #[serde(default)]
    open_web: Option<bool>,
    #[serde(default)]
    sandbox_network: Option<autonoetic_types::agent::SandboxNetworkPolicy>,
    #[serde(default)]
    egress: Option<AgentEgressManifest>,
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

        validate_section_gates(&manifest, &parsed.content)?;

        Ok((manifest, parsed.content))
    }
}

/// Fail a `SKILL.md` whose section gates cannot do what they claim (RFC P3).
///
/// This is the reason gates live in frontmatter rather than as inline markers.
/// An inline marker can drift from a renamed heading and simply stop matching —
/// the section then silently loads always, or never, with nothing to notice it.
/// Both failure modes are caught here instead, at parse time, where the message
/// can name the agent:
///
/// - a gate naming a heading the body does not contain;
/// - a gate naming a phase fact the gateway never derives.
fn validate_section_gates(manifest: &AgentManifest, body: &str) -> anyhow::Result<()> {
    if manifest.sections.is_empty() {
        return Ok(());
    }

    let headings = crate::runtime::context::top_level_headings(body);
    for gate in &manifest.sections {
        let Some(fact) = gate.phase_fact() else {
            anyhow::bail!(
                "agent '{}': section gate for '{}' has an unparseable `when` ({:?}); \
                 expected `phase(<fact>)`",
                manifest.agent.id,
                gate.heading,
                gate.when
            );
        };
        anyhow::ensure!(
            crate::runtime::guidance::ALL_PHASE_FACTS.contains(&fact),
            "agent '{}': section gate for '{}' names unknown phase fact '{}'; known facts: {}",
            manifest.agent.id,
            gate.heading,
            fact,
            crate::runtime::guidance::ALL_PHASE_FACTS.join(", ")
        );
        anyhow::ensure!(
            headings.iter().any(|h| h == gate.heading.trim()),
            "agent '{}': section gate names heading '{}', which is not a top-level \
             `## ` section of the body. Found: {}",
            manifest.agent.id,
            gate.heading,
            if headings.is_empty() {
                "(none)".to_string()
            } else {
                headings.join(" | ")
            }
        );
    }
    Ok(())
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
        singleton: false,
        resident_idle_ttl_secs: None,
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

    let capabilities_inferred =
        meta.capabilities.as_ref().map_or(true, |c| c.is_empty()) && !allowed_tools.is_empty();
    let capabilities = if capabilities_inferred {
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
            // Recorded here — the only place that knows — so downstream trust
            // decisions (skill_install's strict clamp) never guess
            // inferred-vs-declared from the capability set's shape.
            capabilities_inferred,
        })
    } else {
        None
    };

    // External (AgentSkills) imports rarely declare `io.returns`. Give them a
    // default envelope so they hand off a predictable shape and inherit the
    // centralized Output Contract instruction (#481). The synthesized default is
    // a guess about a skill we don't control, so force **advisory** enforcement
    // (preserving any explicit choice) — a non-conforming reply is surfaced as a
    // hint, never blocked, regardless of execution_mode (which would otherwise
    // default script agents to strict).
    let mut io = if agentskills_import.is_some() {
        let mut io = meta.io.unwrap_or_default();
        if io.returns.is_none() {
            io.returns = Some(default_imported_returns_schema());
            io.returns_enforcement = io.returns_enforcement.or(Some(IoReturnsEnforcement::Advisory));
        }
        Some(io)
    } else {
        meta.io
    };

    let execution_mode = meta.execution_mode.unwrap_or_default();

    // RFC C.2 (#770): gateway-injected `anomalies` witness contract. Script
    // agents are excluded — deterministic outputs can't witness/report
    // meaningfully. Unconditional rather than config-gated: this manifest
    // loader has 18+ call sites (CLI, tools, repository) with no
    // `GatewayConfig` in scope, so plumbing a flag through is invasive; the
    // real rollout valve is that reasoning agents default to Advisory
    // enforcement (`IoReturnsEnforcement::effective_returns_enforcement`), so
    // absence never blocks until an operator opts a specific agent into
    // strict.
    if execution_mode == ExecutionMode::Reasoning {
        if let Some(returns) = io.as_mut().and_then(|io| io.returns.as_mut()) {
            inject_anomalies_contract(returns);
        }
    }

    AgentManifest {
        version: meta.version.unwrap_or_else(|| "1.0".to_string()),
        runtime,
        agent,
        capabilities,
        llm_preset: meta.llm_preset,
        llm_overrides: meta.llm_overrides,
        llm_config: meta.llm_config,
        limits: meta.limits,
        background: meta.background,
        disclosure: meta.disclosure,
        io,
        middleware: meta.middleware,
        execution_mode,
        script_entry: meta.script_entry,
        script_input_mode: meta.script_input_mode.unwrap_or_default(),
        gateway_url: meta.gateway_url,
        gateway_token: meta.gateway_token,
        allowed_tool_tiers: meta.allowed_tool_tiers.unwrap_or_default(),
        excluded_tools: meta.excluded_tools.unwrap_or_default(),
        sections: meta.sections.unwrap_or_default(),
        agentskills_import,
        compression: meta.compression,
        open_web: meta.open_web.unwrap_or(false),
        sandbox_network: meta.sandbox_network.unwrap_or_default(),
        egress: meta.egress,
        }
}

/// Permissive default `io.returns` envelope for imported external skills that
/// declare no schema. Injected with forced advisory enforcement (see caller), it
/// nudges the skill toward a predictable handoff shape without blocking output.
fn default_imported_returns_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "status": {
                "type": "string",
                "description": "Outcome of the turn, e.g. ok | partial | failed | clarification_needed."
            },
            "summary": {
                "type": "string",
                "description": "Human-readable result or answer (prose)."
            },
            "result": {
                "type": "object",
                "description": "Optional structured facts for downstream agents."
            }
        },
        "required": ["status", "summary"]
    })
}

/// Injects the gateway-owned `anomalies` witness field into a declared
/// `io.returns` schema, at the single manifest-load choke point
/// (`map_standard_frontmatter_to_manifest`) so the response-validation gate
/// and the Output Contract prompt renderer (`context.rs`) see the same
/// augmented schema. "Anything unexpected?" becomes a schema field, not a
/// virtue: absence is a schema violation, not just a missed nudge — but for
/// reasoning agents it's Advisory by default (RFC C.2), so absence logs +
/// emits a causal event and never blocks. A manifest that already declares
/// its own `anomalies` property wins untouched: no overwrite, no `required`
/// duplication.
fn inject_anomalies_contract(schema: &mut serde_json::Value) {
    let Some(obj) = schema.as_object_mut() else {
        return;
    };
    let skip = obj
        .get("properties")
        .and_then(|p| p.as_object())
        .map_or(true, |props| props.is_empty() || props.contains_key("anomalies"));
    if skip {
        return;
    }

    if let Some(props) = obj.get_mut("properties").and_then(|p| p.as_object_mut()) {
        props.insert("anomalies".to_string(), anomalies_property_schema());
    }

    let required = obj
        .entry("required".to_string())
        .or_insert_with(|| serde_json::Value::Array(Vec::new()));
    if let Some(arr) = required.as_array_mut() {
        if !arr.iter().any(|v| v.as_str() == Some("anomalies")) {
            arr.push(serde_json::Value::String("anomalies".to_string()));
        }
    }
}

fn anomalies_property_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "array",
        "description": "Standing witness contract: anything unexpected or concerning observed while completing this task (empty array if nothing). For serious observations also file an anomaly_flag.",
        "items": {
            "type": "object",
            "properties": {
                "observation": { "type": "string" },
                "subject_ref": { "type": "string" },
                "severity": { "type": "string", "enum": ["low", "medium", "high", "critical"] }
            },
            "required": ["observation"]
        }
    })
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
/// Maps known AgentSkills tool names to Autonoetic capability types.
///
/// Inference proposes narrow capabilities only; it may never mint a
/// wildcard capability from third-party frontmatter text (RFC Part C,
/// docs/design/agent-genesis-one-door.md — "capability is never inferred
/// into wildcards from untrusted text"). Wildcard power must always be an
/// explicit, visible declaration under `metadata.autonoetic.capabilities`
/// that the promotion gate can weigh — a grant minted from a tool-name
/// mapping table has nobody to attribute it to (Ri-0.11).
///
/// - `Bash` / `Bash(...)` → `SandboxFunctions` for the named prefixes only
///   (or `*` for bare `Bash` / `Bash(*)`). **Never** `CodeExecution`: shell
///   execution requires an explicit `CodeExecution` declaration.
/// - `Read`/`View` → covered by baseline `ReadAccess`
/// - `Write`/`Edit` → `WriteAccess`
/// - `WebSearch`/`WebFetch`/`Fetch` → `NetworkAccess` with an **empty**
///   hosts list (deny-all until an operator or the gate explicitly widens
///   it) — never `hosts: ["*"]`.
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
        // A bare `Bash` is the Claude-style "all shell" form, equivalent to
        // `Bash(*)`; `Bash(a:*|b)` names scoped prefixes. Both are the same
        // request — keep this in lockstep with `capability_inference_warnings`.
        let bash_inner = if t == "Bash" {
            Some("")
        } else {
            t.strip_prefix("Bash(").and_then(|s| s.strip_suffix(')'))
        };
        if let Some(rest) = bash_inner {
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
        // No CodeExecution here (RFC Part C): shell execution requires an
        // explicit metadata.autonoetic.capabilities declaration, not a
        // Bash(...) mention in allowed-tools.
    }

    if has_write {
        caps.push(Capability::WriteAccess {
            scopes: vec!["self.*".into(), "skills/*".into()],
        });
    }

    if has_network {
        // Empty hosts, not "*" (RFC Part C): deny-all until an operator or
        // the gate explicitly widens it with concrete hosts.
        caps.push(Capability::NetworkAccess { hosts: vec![] });
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
        // RFC Part C clamp: allowed-tools inference never mints CodeExecution
        // or a wildcard NetworkAccess — only an explicit declaration may.
        assert!(
            !caps.iter().any(|c| matches!(c, Capability::CodeExecution { .. })),
            "inference must not mint CodeExecution from Bash in allowed-tools, got: {caps:?}"
        );
        assert!(
            caps.iter().any(|c| matches!(
                c,
                Capability::NetworkAccess { hosts } if hosts.is_empty()
            )),
            "inference must produce an empty-hosts NetworkAccess, not a wildcard, got: {caps:?}"
        );

        // Imported skill with no declared schema gets a default io.returns
        // envelope with enforcement FORCED to advisory (not mode-derived), so it
        // never blocks even if the skill were script-mode.
        let io = manifest.io.expect("imported skill should get a default io");
        let returns = io.returns.as_ref().expect("default io.returns envelope");
        let props = returns
            .get("properties")
            .and_then(|p| p.as_object())
            .expect("envelope properties");
        assert!(props.contains_key("status") && props.contains_key("summary"));
        assert_eq!(
            io.returns_enforcement,
            Some(autonoetic_types::agent::IoReturnsEnforcement::Advisory),
            "synthesized default must be explicitly advisory, not mode-derived"
        );
    }

    #[test]
    fn test_native_skill_without_io_keeps_none() {
        // Native (non-AgentSkills) skills are not given a default envelope.
        let content = r#"---
name: "simple-native"
description: "native, no io"
metadata:
  autonoetic:
    agent:
      id: "simple-native"
      name: "Simple"
      description: "native"
---
# Simple
"#;
        let (manifest, _body) = SkillParser::parse(content).expect("should parse");
        assert!(manifest.agentskills_import.is_none());
        assert!(manifest.io.is_none(), "native skill without io stays None");
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

    // RFC C.2 (#770): gateway-injected `anomalies` witness contract.

    #[test]
    fn test_anomalies_injected_for_reasoning_agent_with_object_returns() {
        let content = r#"---
name: "reasoning-agent"
description: "A reasoning agent with io.returns"
metadata:
  autonoetic:
    io:
      returns:
        type: object
        required:
          - status
        properties:
          status:
            type: string
---
# Reasoning Agent
"#;
        let (manifest, _body) = SkillParser::parse(content).expect("should parse");
        let returns = manifest
            .io
            .expect("io should parse")
            .returns
            .expect("returns should exist");
        let props = returns
            .get("properties")
            .and_then(|p| p.as_object())
            .expect("properties");
        assert!(props.contains_key("anomalies"), "anomalies should be injected");
        let required = returns
            .get("required")
            .and_then(|r| r.as_array())
            .expect("required array");
        assert!(
            required.iter().any(|v| v.as_str() == Some("anomalies")),
            "anomalies should be added to required"
        );
        // status (declared) untouched.
        assert!(required.iter().any(|v| v.as_str() == Some("status")));
    }

    #[test]
    fn test_anomalies_not_injected_for_script_agent() {
        let content = r#"---
name: "script-agent"
description: "A script agent with io.returns"
metadata:
  autonoetic:
    execution_mode: script
    script_entry: scripts/run.py
    io:
      returns:
        type: object
        required:
          - status
        properties:
          status:
            type: string
---
# Script Agent
"#;
        let (manifest, _body) = SkillParser::parse(content).expect("should parse");
        let returns = manifest
            .io
            .expect("io should parse")
            .returns
            .expect("returns should exist");
        let props = returns
            .get("properties")
            .and_then(|p| p.as_object())
            .expect("properties");
        assert!(
            !props.contains_key("anomalies"),
            "script agents are excluded from the anomalies contract"
        );
        let required = returns.get("required").and_then(|r| r.as_array());
        assert!(
            required.map_or(true, |a| !a.iter().any(|v| v.as_str() == Some("anomalies"))),
            "script agents must not get anomalies added to required"
        );
    }

    #[test]
    fn test_anomalies_not_injected_without_io_returns() {
        let content = r#"---
name: "no-returns-agent"
description: "A reasoning agent with no io.returns"
metadata:
  autonoetic:
    agent:
      id: "no-returns-agent"
      name: "No Returns"
      description: "no io"
---
# No Returns Agent
"#;
        let (manifest, _body) = SkillParser::parse(content).expect("should parse");
        assert!(manifest.io.is_none(), "no io.returns declared, io stays None");
    }

    #[test]
    fn test_anomalies_left_untouched_when_manifest_declares_own() {
        let content = r#"---
name: "self-witness-agent"
description: "A reasoning agent that declares its own anomalies field"
metadata:
  autonoetic:
    io:
      returns:
        type: object
        required:
          - status
        properties:
          status:
            type: string
          anomalies:
            type: string
            description: "custom, non-array anomalies field"
---
# Self Witness Agent
"#;
        let (manifest, _body) = SkillParser::parse(content).expect("should parse");
        let returns = manifest
            .io
            .expect("io should parse")
            .returns
            .expect("returns should exist");
        let anomalies_schema = returns
            .get("properties")
            .and_then(|p| p.get("anomalies"))
            .expect("anomalies property should exist");
        assert_eq!(
            anomalies_schema.get("type").and_then(|t| t.as_str()),
            Some("string"),
            "manifest's own anomalies declaration must not be overwritten"
        );
        let required = returns
            .get("required")
            .and_then(|r| r.as_array())
            .expect("required array");
        let anomalies_count = required
            .iter()
            .filter(|v| v.as_str() == Some("anomalies"))
            .count();
        assert_eq!(
            anomalies_count, 0,
            "manifest did not list anomalies in required; gateway must not add it either"
        );
    }

    #[test]
    fn test_anomalies_not_injected_without_properties_object() {
        let content = r#"---
name: "bare-schema-agent"
description: "A reasoning agent with a schema lacking a properties object"
metadata:
  autonoetic:
    io:
      returns:
        type: string
---
# Bare Schema Agent
"#;
        let (manifest, _body) = SkillParser::parse(content).expect("should parse");
        let returns = manifest
            .io
            .expect("io should parse")
            .returns
            .expect("returns should exist");
        assert!(
            returns.get("properties").is_none(),
            "schema without properties stays untouched"
        );
        assert!(returns.get("required").is_none());
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

    /// A bare `Bash` entry (Claude-style "all shell") is equivalent to
    /// `Bash(*)`: wildcard `SandboxFunctions`, not a literal `"Bash"` prefix,
    /// and never `CodeExecution`.
    #[test]
    fn test_infer_capabilities_bare_bash_is_wildcard() {
        let caps = infer_capabilities(&["Bash".to_string()]);
        let sandbox = caps
            .iter()
            .find_map(|c| match c {
                Capability::SandboxFunctions { allowed } => Some(allowed),
                _ => None,
            })
            .expect("bare Bash should infer SandboxFunctions");
        assert_eq!(sandbox, &vec!["*".to_string()], "bare Bash must map to wildcard, not [\"Bash\"]");
        assert!(
            !caps.iter().any(|c| matches!(c, Capability::CodeExecution { .. })),
            "bare Bash must not infer CodeExecution, got: {caps:?}"
        );
    }

    /// RFC Part C: `Bash(...)` in allowed-tools proposes `SandboxFunctions`
    /// only; `CodeExecution` must never be inferred — a skill that needs
    /// shell execution must declare `CodeExecution` explicitly.
    #[test]
    fn test_infer_capabilities_bash_never_mints_code_execution() {
        let caps = infer_capabilities(&["Bash(*)".to_string()]);
        assert!(
            !caps
                .iter()
                .any(|c| matches!(c, Capability::CodeExecution { .. })),
            "Bash(*) must not infer CodeExecution, got: {caps:?}"
        );
        assert!(caps
            .iter()
            .any(|c| matches!(c, Capability::SandboxFunctions { .. })));
    }

    /// RFC Part C: `WebSearch`/`WebFetch`/`Fetch` infer `NetworkAccess` with
    /// an empty hosts list (deny-all) — never `hosts: ["*"]`.
    #[test]
    fn test_infer_capabilities_network_empty_hosts_never_wildcard() {
        for tool in ["WebSearch", "WebFetch", "Fetch"] {
            let caps = infer_capabilities(&[tool.to_string()]);
            let network = caps
                .iter()
                .filter_map(|c| match c {
                    Capability::NetworkAccess { hosts } => Some(hosts),
                    _ => None,
                })
                .next()
                .unwrap_or_else(|| panic!("{tool} should infer NetworkAccess"));
            assert!(
                network.is_empty(),
                "{tool} must infer empty hosts, not a wildcard, got: {network:?}"
            );
        }
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

    #[test]
    fn test_parse_egress_output_label_floor() {
        let content = r#"---
name: "email-agent"
description: "An email-reading agent with a local_only floor"
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
      id: "email-agent"
      name: "Email Agent"
      description: "Reads local emails"
    egress:
      output_label: local_only
---
# Email Agent
"#;
        let (manifest, _body) = SkillParser::parse(content).expect("should parse");
        let egress = manifest.egress.expect("egress manifest should be present");
        assert_eq!(
            egress.output_label,
            Some(autonoetic_types::egress::NamedEgressLabel::LocalOnly)
        );
    }

    #[test]
    fn test_parse_egress_absent_when_not_declared() {
        let content = r#"---
name: "plain-agent"
description: "No egress declaration"
metadata:
  autonoetic:
    agent:
      id: "plain-agent"
      name: "Plain"
      description: "no egress"
---
# Plain
"#;
        let (manifest, _body) = SkillParser::parse(content).expect("should parse");
        assert!(manifest.egress.is_none());
    }
}

#[cfg(test)]
mod section_gate_validation_tests {
    use super::*;

    fn skill(gates: &str, body: &str) -> String {
        format!(
            "---\nname: t\ndescription: d\nmetadata:\n  autonoetic:\n    agent:\n      id: \"t.default\"\n      name: t\n      description: d\n{gates}---\n{body}"
        )
    }

    const GATES: &str = "    sections:\n      - heading: \"Federation\"\n        when: phase(artifact_built)\n";

    #[test]
    fn valid_gate_parses() {
        let (m, _) = SkillParser::parse(&skill(GATES, "## Federation\nbody\n")).expect("should parse");
        assert_eq!(m.sections.len(), 1);
        assert_eq!(m.sections[0].phase_fact(), Some("artifact_built"));
    }

    #[test]
    fn gate_naming_a_missing_heading_fails_at_parse_time() {
        // The whole reason gates live in frontmatter: a renamed heading must be
        // loud here, not silently stop gating at runtime.
        let err = SkillParser::parse(&skill(GATES, "## Renamed\nbody\n")).unwrap_err().to_string();
        assert!(err.contains("not a top-level"), "got: {err}");
        assert!(err.contains("Renamed"), "error should list what it did find: {err}");
    }

    #[test]
    fn gate_naming_an_unknown_fact_fails_at_parse_time() {
        let gates = "    sections:\n      - heading: \"Federation\"\n        when: phase(artifact_build)\n";
        let err = SkillParser::parse(&skill(gates, "## Federation\nbody\n")).unwrap_err().to_string();
        assert!(err.contains("unknown phase fact"), "got: {err}");
        assert!(err.contains("artifact_built"), "error should list known facts: {err}");
    }

    #[test]
    fn malformed_when_fails_at_parse_time() {
        let gates = "    sections:\n      - heading: \"Federation\"\n        when: always\n";
        let err = SkillParser::parse(&skill(gates, "## Federation\nbody\n")).unwrap_err().to_string();
        assert!(err.contains("unparseable"), "got: {err}");
    }

    #[test]
    fn subsection_heading_does_not_satisfy_a_gate() {
        // `### Federation` is not gateable — a gate must name a top-level section
        // so it unambiguously carries its subsections.
        let err = SkillParser::parse(&skill(GATES, "## Top\n### Federation\nbody\n")).unwrap_err().to_string();
        assert!(err.contains("not a top-level"), "got: {err}");
    }
}
