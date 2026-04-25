//! `skill.install` — fetch a remote SKILL.md and register it as a local agent.
//!
//! Requires the `SkillInstall` capability. The `allowed_sources` field constrains
//! which URL hosts the agent may pull from.

use crate::llm::ToolDefinition;
use crate::policy::PolicyEngine;
use crate::runtime::active_execution_registry::NativeToolRunContext;
use crate::runtime::tools::{extract_host, NativeTool, NativeToolRegistry};
use autonoetic_types::agent::{AgentIdentity, AgentManifest, LlmConfig};
use autonoetic_types::capability::Capability;
use serde::Deserialize;
use std::path::Path;

pub fn register_tools(registry: &mut NativeToolRegistry) {
    registry.register(Box::new(SkillInstallTool));
}

pub struct SkillInstallTool;

#[derive(Debug, Deserialize)]
struct SkillInstallArgs {
    /// URL to a remote SKILL.md file.
    url: String,
    /// New agent ID to register the skill as.
    agent_id: String,
    /// Trust level applied to the imported capabilities.
    /// - "generous": capabilities from the SKILL.md are used as-is.
    /// - "strict" (default): capabilities preserved but every action requires approval.
    /// - "audit": read-only + approval gate, ignores original capabilities.
    #[serde(default)]
    trust_mode: Option<String>,
}

impl NativeTool for SkillInstallTool {
    fn name(&self) -> &'static str {
        "skill_install"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: "Fetch a remote SKILL.md and register it as a new local agent. \
                Requires the SkillInstall capability. The agent directory is created under \
                agents_dir, the skill is parsed and a runtime.lock is generated, then the \
                agent is immediately bootstrapped and promoted to active. \
                trust_mode controls how original capabilities are treated: \
                generous (keep as-is), strict (add approval gate, default), \
                audit (read-only + approval)."
                .to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "url": {
                        "type": "string",
                        "description": "Full URL to a SKILL.md file, e.g. https://agentskills.io/skills/web-researcher/SKILL.md"
                    },
                    "agent_id": {
                        "type": "string",
                        "description": "ID for the newly installed agent, e.g. \"web-researcher.default\". \
                            May only contain ASCII letters, digits, '.', '-', and '_'."
                    },
                    "trust_mode": {
                        "type": "string",
                        "enum": ["generous", "strict", "audit"],
                        "description": "How to treat the imported capabilities. \
                            generous: use as declared; \
                            strict (default): add approval requirement to all actions; \
                            audit: drop to read-only + approval gate."
                    }
                },
                "required": ["url", "agent_id"],
                "additionalProperties": false
            }),
        }
    }

    fn is_available(&self, manifest: &AgentManifest) -> bool {
        manifest
            .capabilities
            .iter()
            .any(|c| matches!(c, Capability::SkillInstall { .. }))
    }

    fn execute(
        &self,
        _manifest: &AgentManifest,
        policy: &PolicyEngine,
        _agent_dir: &Path,
        gateway_dir: Option<&Path>,
        arguments_json: &str,
        _session_id: Option<&str>,
        _turn_id: Option<&str>,
        config: Option<&autonoetic_types::config::GatewayConfig>,
        _gateway_store: Option<std::sync::Arc<crate::scheduler::gateway_store::GatewayStore>>,
        _run_context: Option<&NativeToolRunContext>,
    ) -> anyhow::Result<String> {
        let args: SkillInstallArgs = serde_json::from_str(arguments_json)
            .map_err(|e| anyhow::anyhow!("Invalid arguments: {}", e))?;

        // ── 1. Validate agent_id ──────────────────────────────────────────────
        crate::runtime::tools::validate_agent_id(&args.agent_id)?;

        // ── 2. Policy: SkillInstall capability must permit this URL host ──────
        let url_host = extract_host(&args.url)?;
        if !policy.can_install_skill(&url_host) {
            return Ok(serde_json::json!({
                "ok": false,
                "error": format!(
                    "SkillInstall capability does not permit fetching from host '{}'",
                    url_host
                ),
            })
            .to_string());
        }

        // ── 3. Resolve config and paths ───────────────────────────────────────
        let config = config.ok_or_else(|| anyhow::anyhow!("Gateway config not available"))?;
        let gateway_dir =
            gateway_dir.ok_or_else(|| anyhow::anyhow!("Gateway directory not available"))?;

        // Use dot-to-dash conversion for the filesystem directory name (same as CLI).
        let dir_name = args.agent_id.replace('.', "-");
        let target_dir = config.agents_dir.join(&dir_name);

        anyhow::ensure!(
            !target_dir.exists(),
            "Agent directory '{}' already exists — choose a different agent_id or remove the existing agent first",
            target_dir.display()
        );

        // ── 4. Fetch the remote SKILL.md ──────────────────────────────────────
        let url_clone = args.url.clone();
        let (http_status, skill_content) = {
            let result: anyhow::Result<(u16, String)> = (|| {
                let client = reqwest::blocking::Client::builder()
                    .timeout(std::time::Duration::from_secs(15))
                    .build()?;
                let resp = client.get(url_clone.as_str()).send()?;
                let status = resp.status().as_u16();
                if !resp.status().is_success() {
                    let _ = resp.text()?;
                    return Ok((status, String::new()));
                }
                let content = resp.text()?;
                Ok((status, content))
            })();
            result?
        };

        if !(200..300).contains(&(http_status as i32)) {
            return Ok(serde_json::json!({
                "ok": false,
                "error": format!("HTTP {} fetching SKILL.md from {}", http_status, args.url),
            })
            .to_string());
        }

        // ── 5. Parse the SKILL.md ─────────────────────────────────────────────
        let (parsed_manifest, body) = crate::runtime::parser::SkillParser::parse(&skill_content)
            .map_err(|e| {
                anyhow::anyhow!("Failed to parse remote SKILL.md from {}: {}", args.url, e)
            })?;

        // ── 6. Apply trust mode ───────────────────────────────────────────────
        let trust_mode = args.trust_mode.as_deref().unwrap_or("strict");
        let capabilities = apply_trust_mode(trust_mode, &parsed_manifest)?;

        // ── 7. Build target manifest ──────────────────────────────────────────
        let llm_config = parsed_manifest.llm_config.clone().or_else(|| {
            // Fall back to the gateway's default preset if the skill has no LLM config.
            config
                .llm_preset_mapping
                .get("default")
                .and_then(|name| config.llm_presets.get(name.as_str()))
                .map(|preset| LlmConfig {
                    provider: preset
                        .provider
                        .clone()
                        .unwrap_or_else(|| "anthropic".to_string()),
                    model: preset
                        .model
                        .clone()
                        .unwrap_or_else(|| "claude-sonnet-4-6".to_string()),
                    temperature: preset.temperature.unwrap_or(0.2),
                    fallback_provider: None,
                    fallback_model: None,
                    chat_only: preset.chat_only.unwrap_or(false),
                    context_window_tokens: None,
                    base_url: preset.base_url.clone(),
                    api_key_env: preset.api_key_env.clone(),
                    routing_preset: None,
                    thinking: preset.thinking.clone(),
                })
        });

        let target_manifest = AgentManifest {
            version: parsed_manifest.version.clone(),
            runtime: parsed_manifest.runtime.clone(),
            agent: AgentIdentity {
                id: args.agent_id.clone(),
                name: parsed_manifest.agent.name.clone(),
                description: parsed_manifest.agent.description.clone(),
            },
            capabilities,
            llm_config,
            limits: parsed_manifest.limits.clone(),
            background: parsed_manifest.background.clone(),
            disclosure: parsed_manifest.disclosure.clone(),
            io: parsed_manifest.io.clone(),
            middleware: parsed_manifest.middleware.clone(),
            execution_mode: parsed_manifest.execution_mode,
            script_entry: parsed_manifest.script_entry.clone(),
            script_input_mode: parsed_manifest.script_input_mode,
            gateway_url: None,
            gateway_token: None,
            response_contract: parsed_manifest.response_contract.clone(),
            allowed_tool_tiers: parsed_manifest.allowed_tool_tiers.clone(),
            agentskills_import: parsed_manifest.agentskills_import.clone(),
            compression: parsed_manifest.compression.clone(),
        };

        // ── 8. Write agent directory: SKILL.md + runtime.lock ─────────────────
        std::fs::create_dir_all(&target_dir)?;

        let skill_doc =
            crate::runtime::install_contract::render_skill_document(&target_manifest, &body)?;
        std::fs::write(target_dir.join("SKILL.md"), &skill_doc)?;

        let lock_doc = crate::runtime::install_contract::render_runtime_lock_example();
        std::fs::write(target_dir.join("runtime.lock"), &lock_doc)?;

        tracing::info!(
            target: "skill_install",
            agent_id = %args.agent_id,
            url = %args.url,
            trust_mode = %trust_mode,
            "Wrote agent bundle to disk"
        );

        // ── 9. Bootstrap and auto-promote the new agent ───────────────────────
        let activated = crate::bootstrap::bootstrap_single_agent(config, gateway_dir, &dir_name)?;

        let message = if activated {
            format!("Skill installed and promoted as agent '{}'", args.agent_id)
        } else {
            format!(
                "Skill written to disk as agent '{}' but a matching revision already existed — no new promotion",
                args.agent_id
            )
        };

        tracing::info!(
            target: "skill_install",
            agent_id = %args.agent_id,
            activated = activated,
            "Bootstrap complete"
        );

        Ok(serde_json::json!({
            "ok": true,
            "agent_id": args.agent_id,
            "trust_mode": trust_mode,
            "activated": activated,
            "message": message,
        })
        .to_string())
    }
}

/// Map a trust_mode string to the correct capability set.
fn apply_trust_mode(trust_mode: &str, parsed: &AgentManifest) -> anyhow::Result<Vec<Capability>> {
    match trust_mode {
        "generous" => {
            // Use capabilities declared in the remote SKILL.md as-is.
            // Fall back to minimal defaults if none declared.
            if parsed.capabilities.is_empty() {
                let allowed_tools: Vec<String> = parsed
                    .agentskills_import
                    .as_ref()
                    .map(|m| m.allowed_tools.clone())
                    .unwrap_or_default();
                if allowed_tools.is_empty() {
                    Ok(vec![
                        Capability::ReadAccess {
                            scopes: vec!["self.*".to_string()],
                        },
                        Capability::WriteAccess {
                            scopes: vec!["self.*".to_string()],
                        },
                    ])
                } else {
                    Ok(crate::runtime::parser::infer_capabilities(&allowed_tools))
                }
            } else {
                Ok(parsed.capabilities.clone())
            }
        }
        "strict" => {
            // Preserve capabilities but add an approval gate.
            let mut caps = if parsed.capabilities.is_empty() {
                let allowed_tools: Vec<String> = parsed
                    .agentskills_import
                    .as_ref()
                    .map(|m| m.allowed_tools.clone())
                    .unwrap_or_default();
                if allowed_tools.is_empty() {
                    vec![Capability::ReadAccess {
                        scopes: vec!["self.*".to_string()],
                    }]
                } else {
                    crate::runtime::parser::infer_capabilities(&allowed_tools)
                }
            } else {
                parsed.capabilities.clone()
            };
            caps.push(Capability::ApprovalQueue {
                patterns: vec!["*".to_string()],
            });
            Ok(caps)
        }
        "audit" => {
            // Read-only + approval gate — ignores declared capabilities.
            Ok(vec![
                Capability::ReadAccess {
                    scopes: vec!["self.*".to_string()],
                },
                Capability::ApprovalQueue {
                    patterns: vec!["*".to_string()],
                },
            ])
        }
        other => anyhow::bail!(
            "Unknown trust_mode '{}'; valid values: generous, strict, audit",
            other
        ),
    }
}
