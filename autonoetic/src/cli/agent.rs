use std::io::Write;
use std::path::Path;
use std::sync::Arc;
use tracing::info;

use crate::cli::common::{AgentAliasCommands, AgentCredentialCommands, AgentRevisionCommands};
use autonoetic_gateway::llm::Message;
use autonoetic_gateway::policy::PolicyEngine;
use autonoetic_gateway::runtime::tools::NativeTool;
use autonoetic_types::agent::LlmExchangeUsage;
use autonoetic_types::agent::{AgentIdentity, AgentManifest, ExecutionMode, RuntimeDeclaration};
use autonoetic_types::agent_revision::PromotionKind;
use autonoetic_types::capability::Capability;
use autonoetic_types::config::GatewayConfig;
use autonoetic_types::session_outcome::SessionCloseOutcome;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

/// LLM configuration for template rendering
#[derive(Debug, Clone, Default)]
pub struct LlmTemplateConfig {
    pub provider: String,
    pub model: String,
    pub temperature: f64,
    pub chat_only: bool,
    pub base_url: Option<String>,
    pub api_key_env: Option<String>,
    pub routing_preset: Option<String>,
}

/// Resolve LLM config from CLI flags, presets, or defaults
pub fn resolve_llm_config(
    config: &GatewayConfig,
    template: Option<&str>,
    preset_name: Option<&str>,
    provider: Option<&str>,
    model: Option<&str>,
) -> LlmTemplateConfig {
    // 1. Direct CLI override takes highest priority
    if let Some(p) = provider {
        return LlmTemplateConfig {
            provider: p.to_string(),
            model: model.unwrap_or("gpt-4o").to_string(),
            temperature: 0.2,
            chat_only: false,
            base_url: None,
            api_key_env: None,
            routing_preset: None,
        };
    }

    // Helper to convert preset to template config
    fn preset_to_config(
        preset_name: &str,
        preset: &autonoetic_types::config::LlmPreset,
        all_presets: &std::collections::HashMap<String, autonoetic_types::config::LlmPreset>,
    ) -> LlmTemplateConfig {
        if let Some(ref routing) = preset.routing {
            // Routing preset: resolve first model preset to get concrete provider/model
            if let Some(first_name) = routing.models.first() {
                if let Some(first_preset) = all_presets.get(first_name) {
                    return LlmTemplateConfig {
                        provider: first_preset.provider.clone().unwrap_or_default(),
                        model: first_preset.model.clone().unwrap_or_default(),
                        temperature: preset.temperature.unwrap_or(0.2),
                        chat_only: preset.chat_only.unwrap_or(false),
                        base_url: preset.base_url.clone(),
                        api_key_env: preset
                            .api_key_env
                            .clone()
                            .or(first_preset.api_key_env.clone()),
                        routing_preset: Some(preset_name.to_string()),
                    };
                }
            }
            // Fallback: empty provider/model, but still set routing_preset
            LlmTemplateConfig {
                provider: String::new(),
                model: String::new(),
                temperature: preset.temperature.unwrap_or(0.2),
                chat_only: preset.chat_only.unwrap_or(false),
                base_url: preset.base_url.clone(),
                api_key_env: preset.api_key_env.clone(),
                routing_preset: Some(preset_name.to_string()),
            }
        } else {
            // Fixed preset
            LlmTemplateConfig {
                provider: preset.provider.clone().unwrap_or_default(),
                model: preset.model.clone().unwrap_or_default(),
                temperature: preset.temperature.unwrap_or(0.2),
                chat_only: preset.chat_only.unwrap_or(false),
                base_url: preset.base_url.clone(),
                api_key_env: preset.api_key_env.clone(),
                routing_preset: None,
            }
        }
    }

    // 2. Named preset from config
    if let Some(preset_name) = preset_name {
        if let Some(preset) = config.llm_presets.get(preset_name) {
            return preset_to_config(preset_name, preset, &config.llm_presets);
        }
    }

    // 3. Role-based preset mapping from config (same lookup order as gateway bootstrap)
    if let Some(template_name) = template {
        let mapped_preset_name = config
            .llm_preset_mapping
            .get(template_name)
            .or_else(|| {
                template_name
                    .rsplit_once('.')
                    .and_then(|(base, _)| config.llm_preset_mapping.get(base))
            })
            .or_else(|| config.llm_preset_mapping.get("default"));
        if let Some(mapped_preset_name) = mapped_preset_name {
            if let Some(preset) = config.llm_presets.get(mapped_preset_name.as_str()) {
                return preset_to_config(mapped_preset_name, preset, &config.llm_presets);
            }
        }
    }

    // 3.5. Default preset from config (covers unmapped agents)
    if let Some(preset) = config.llm_presets.get("default") {
        return preset_to_config("default", preset, &config.llm_presets);
    }

    // 4. Hardcoded defaults per template (backward compatible)
    match template.unwrap_or("generic") {
        "planner" => LlmTemplateConfig {
            provider: "anthropic".to_string(),
            model: "claude-sonnet-4-20250514".to_string(),
            temperature: 0.2,
            chat_only: false,
            base_url: None,
            api_key_env: None,
            routing_preset: None,
        },
        "coder" => LlmTemplateConfig {
            provider: "anthropic".to_string(),
            model: "claude-sonnet-4-20250514".to_string(),
            temperature: 0.1,
            chat_only: false,
            base_url: None,
            api_key_env: None,
            routing_preset: None,
        },
        "researcher" => LlmTemplateConfig {
            provider: "openai".to_string(),
            model: "gpt-4o".to_string(),
            temperature: 0.3,
            chat_only: false,
            base_url: None,
            api_key_env: None,
            routing_preset: None,
        },
        "evaluator" | "auditor" => LlmTemplateConfig {
            provider: "openrouter".to_string(),
            model: "google/gemini-3-flash-preview".to_string(),
            temperature: 0.1,
            chat_only: false,
            base_url: None,
            api_key_env: None,
            routing_preset: None,
        },
        _ => LlmTemplateConfig {
            provider: "openai".to_string(),
            model: "gpt-4o".to_string(),
            temperature: 0.2,
            chat_only: false,
            base_url: None,
            api_key_env: None,
            routing_preset: None,
        },
    }
}

pub fn resolve_preset_name_for_init(
    config: &GatewayConfig,
    agent_id: &str,
    template: Option<&str>,
    explicit_preset: Option<&str>,
) -> String {
    if let Some(p) = explicit_preset.filter(|s| !s.trim().is_empty()) {
        return p.to_string();
    }
    if let Some(name) = autonoetic_gateway::runtime::llm_preset_resolver::resolve_preset_name_for_agent(
        agent_id,
        &config.llm_preset_mapping,
    ) {
        return name.to_string();
    }
    if let Some(t) = template {
        if let Some(name) =
            autonoetic_gateway::runtime::llm_preset_resolver::resolve_preset_name_for_agent(
                t,
                &config.llm_preset_mapping,
            )
        {
            return name.to_string();
        }
    }
    config
        .llm_preset_mapping
        .get("default")
        .cloned()
        .unwrap_or_else(|| "fallback".to_string())
}

pub fn init_agent_scaffold(
    config_path: &Path,
    agent_id: &str,
    template: Option<&str>,
    preset: Option<&str>,
    provider: Option<&str>,
    model: Option<&str>,
) -> anyhow::Result<()> {
    anyhow::ensure!(!agent_id.trim().is_empty(), "agent_id must not be empty");

    let mut config = autonoetic_gateway::config::load_config(config_path)?;
    std::fs::create_dir_all(&config.agents_dir)?;

    let agent_dir = config.agents_dir.join(agent_id);
    anyhow::ensure!(
        !agent_dir.exists(),
        "Agent '{}' already exists at {}",
        agent_id,
        agent_dir.display()
    );
    std::fs::create_dir_all(&agent_dir)?;
    std::fs::create_dir_all(agent_dir.join("state"))?;
    std::fs::create_dir_all(agent_dir.join("history"))?;
    std::fs::create_dir_all(agent_dir.join("skills"))?;
    std::fs::create_dir_all(agent_dir.join("scripts"))?;

    let (preset_name, temperature_override) = if let Some(p) = provider {
        let local_name = format!("_local.{agent_id}");
        config.llm_presets.insert(
            local_name.clone(),
            autonoetic_types::config::LlmPreset {
                provider: Some(p.to_string()),
                model: Some(model.unwrap_or("gpt-4o").to_string()),
                temperature: Some(0.2),
                fallback_provider: None,
                fallback_model: None,
                chat_only: None,
                context_window_tokens: None,
                base_url: None,
                tier: None,
                cost: None,
                latency: None,
                api_key_env: None,
                thinking: None,
                routing: None,
            },
        );
        autonoetic_gateway::config::save_config(config_path, &config)?;
        (local_name, None)
    } else {
        (
            resolve_preset_name_for_init(&config, agent_id, template, preset),
            template_temperature_override(template),
        )
    };

    let skill_md = render_skill_template(agent_id, template, &preset_name, temperature_override);
    std::fs::write(agent_dir.join("SKILL.md"), skill_md)?;
    std::fs::write(
        agent_dir.join("runtime.lock"),
        default_runtime_lock_contents(),
    )?;

    println!(
        "Initialized agent '{}' in {} (llm_preset: {})",
        agent_id,
        agent_dir.display(),
        preset_name,
    );
    Ok(())
}

fn template_temperature_override(template: Option<&str>) -> Option<f64> {
    match template.unwrap_or("generic") {
        "coder" | "evaluator" | "auditor" => Some(0.1),
        "researcher" => Some(0.3),
        _ => None,
    }
}

pub fn render_skill_template(
    agent_id: &str,
    template: Option<&str>,
    preset_name: &str,
    temperature_override: Option<f64>,
) -> String {
    let (name_suffix, description, body) = match template.unwrap_or("generic") {
        "planner" => (
            "Planner",
            "Front-door lead agent for ambiguous goals.",
            "You are a planner agent. Interpret ambiguous goals, decide whether to answer directly or structure specialist work, and keep delegation explicit and auditable.",
        ),
        "researcher" => (
            "Researcher",
            "Research-focused autonomous agent.",
            "You are a researcher agent. Build evidence-based outputs and cite sources.",
        ),
        "coder" => (
            "Coder",
            "Software engineering autonomous agent.",
            "You are a coding agent. Produce tested, minimal, and auditable changes.",
        ),
        "auditor" => (
            "Auditor",
            "Audit and review autonomous agent.",
            "You are an auditor agent. Prioritize correctness, risks, and reproducibility.",
        ),
        _ => (
            "Agent",
            "General-purpose autonomous agent.",
            "You are an autonomous agent. Plan clearly and execute safely.",
        ),
    };
    let overrides_block = temperature_override
        .map(|t| format!("\n    llm_overrides:\n      temperature: {t}"))
        .unwrap_or_default();

    format!(
        r#"---
name: "{agent_id}"
description: "{description}"
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
      id: "{agent_id}"
      name: "{agent_id} {name_suffix}"
      description: "{description}"
    llm_preset: {preset}{overrides}
---
# {agent_id}

{body}
"#,
        preset = preset_name,
        overrides = overrides_block,
    )
}

pub fn default_runtime_lock_contents() -> String {
    use autonoetic_gateway::runtime::install_contract::{
        gateway_version, sdk_version, GATEWAY_BUILD_SHA256, GATEWAY_BUILD_TAG,
    };
    format!(
        r#"gateway:
  artifact: "marketplace://gateway/autonoetic-gateway"
  version: "{gw_ver}"
  sha256: "{sha}"
  build_tag: "{build_tag}"
sdk:
  version: "{sdk_ver}"
sandbox:
  backend: "bubblewrap"
dependencies: []
artifacts: []
layers: []
"#,
        gw_ver = gateway_version(),
        sha = GATEWAY_BUILD_SHA256,
        build_tag = GATEWAY_BUILD_TAG,
        sdk_ver = sdk_version(),
    )
}

pub fn handle_agent_presets(config_path: &Path) -> anyhow::Result<()> {
    let config = autonoetic_gateway::config::load_config(config_path)?;

    if config.llm_presets.is_empty() {
        println!("No LLM presets configured. Add presets to config.yaml:");
        println!();
        println!("llm_presets:");
        println!("  agentic:");
        println!("    provider: anthropic");
        println!("    model: claude-sonnet-4-20250514");
        println!("    temperature: 0.2");
        println!("  coding:");
        println!("    provider: anthropic");
        println!("    model: claude-sonnet-4-20250514");
        println!("    temperature: 0.1");
        println!();
        println!("Then map templates to presets:");
        println!();
        println!("llm_preset_mapping:");
        println!("  planner: agentic");
        println!("  coder: coding");
        println!("  researcher: agentic");
        return Ok(());
    }

    println!(
        "{:<20} {:<30} {:<15} {}",
        "PRESET", "PROVIDER", "MODEL", "TEMP"
    );
    println!("{}", "-".repeat(80));

    for (name, preset) in &config.llm_presets {
        let temp = preset.temperature.unwrap_or(0.0);
        let provider = preset.provider.as_deref().unwrap_or("(routing preset)");
        let model = preset.model.as_deref().unwrap_or_else(|| {
            preset
                .routing
                .as_ref()
                .and_then(|r| r.models.first())
                .map(|s| s.as_str())
                .unwrap_or("")
        });
        let type_tag = if preset.routing.is_some() {
            " [routing]"
        } else {
            ""
        };
        println!(
            "{:<20} {:<30} {:<15} {:.1}{}",
            name, provider, model, temp, type_tag
        );
    }

    if !config.llm_preset_mapping.is_empty() {
        println!();
        println!("Template → Preset mappings:");
        for (template, preset_name) in &config.llm_preset_mapping {
            println!("  {} → {}", template, preset_name);
        }
    }

    Ok(())
}

const DEFAULT_CONFIG_TEMPLATE: &str = include_str!("../../../config/config-template.yaml");

pub fn handle_init_config(output: Option<&str>, overwrite: bool) -> anyhow::Result<()> {
    let output_path = output.unwrap_or("config.yaml");
    let path = std::path::Path::new(output_path);

    if path.exists() && !overwrite {
        anyhow::bail!(
            "Config file already exists at {}. Use --overwrite to replace it.",
            path.display()
        );
    }

    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }

    // Resolve absolute path for agents_dir
    let config_dir = path.parent().unwrap_or(std::path::Path::new("."));
    let agents_dir = config_dir.join("agents");
    let agents_dir_str = agents_dir.display().to_string();

    let config_content = DEFAULT_CONFIG_TEMPLATE.replace(
        "# agents_dir is set to absolute path based on config location",
        &format!("agents_dir: \"{}\"", agents_dir_str),
    );

    std::fs::write(path, config_content)?;
    println!("Created config file at {}", path.display());
    println!("  agents_dir: {}", agents_dir_str);
    println!();
    println!("Next steps:");
    println!("  1. Edit the file to set your LLM provider and API keys");
    println!(
        "  2. Bootstrap agents: autonoetic agent bootstrap --config {}",
        path.display()
    );
    println!(
        "  3. Start gateway: autonoetic gateway start --config {}",
        path.display()
    );
    println!();
    println!(
        "Tip: Use 'autonoetic agent presets --config {}' to list configured presets.",
        path.display()
    );

    Ok(())
}

pub async fn handle_agent_list(config_path: &Path) -> anyhow::Result<()> {
    let config = autonoetic_gateway::config::load_config(config_path)?;
    let aliases = list_alias_rows_from_registry(&config)?;
    if aliases.is_empty() {
        println!("No aliases found in {}", config.agents_dir.display());
    } else {
        println!(
            "{:<28} {:<28} {:<30} {:<10} UPDATED AT",
            "ALIAS ID", "AGENT ID", "ACTIVE REVISION", "STATUS"
        );
        for row in aliases {
            println!(
                "{:<28} {:<28} {:<30} {:<10} {}",
                row.alias_id, row.agent_id, row.active_revision, row.status, row.updated_at
            );
        }
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct AliasListRow {
    alias_id: String,
    agent_id: String,
    active_revision: String,
    status: String,
    updated_at: String,
}

fn list_alias_rows_from_registry(config: &GatewayConfig) -> anyhow::Result<Vec<AliasListRow>> {
    let gateway_dir = autonoetic_gateway::execution::gateway_root_dir(config);
    let store = autonoetic_gateway::scheduler::gateway_store::GatewayStore::open(&gateway_dir)?;
    let mut rows = Vec::new();
    for alias in store.list_agent_aliases(None)? {
        let revision = store
            .get_agent_revision(&alias.revision_id)?
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "Alias '{}' points to missing revision '{}'",
                    alias.alias_id,
                    alias.revision_id
                )
            })?;
        let active_revision = if revision.short_id.is_empty() {
            alias.revision_id.clone()
        } else {
            format!("rev_{}", revision.short_id)
        };
        rows.push(AliasListRow {
            alias_id: alias.alias_id,
            agent_id: alias.agent_id,
            active_revision,
            status: format!("{:?}", revision.status),
            updated_at: alias.updated_at,
        });
    }
    Ok(rows)
}

fn admin_revision_manifest() -> AgentManifest {
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
            id: "cli.admin".to_string(),
            name: "CLI Admin".to_string(),
            description: "CLI administrative operator".to_string(),
        },
        capabilities: vec![Capability::AgentRevision {
            patterns: vec!["*".to_string()],
        }],
        llm_preset: None,
        llm_overrides: None,
        llm_config: None,
        limits: None,
        background: None,
        disclosure: None,
        io: None,
        middleware: None,
        execution_mode: ExecutionMode::Reasoning,
        script_entry: None,
        script_input_mode: Default::default(),
        gateway_url: None,
        gateway_token: None,
        allowed_tool_tiers: vec![],
        agentskills_import: None,
        compression: None,
            open_web: false,
        sandbox_network: Default::default(),
    }
}

pub fn handle_agent_revision(
    config_path: &Path,
    command: &AgentRevisionCommands,
) -> anyhow::Result<()> {
    let config = autonoetic_gateway::config::load_config(config_path)?;
    let gateway_dir = autonoetic_gateway::execution::gateway_root_dir(&config);
    let store = std::sync::Arc::new(
        autonoetic_gateway::scheduler::gateway_store::GatewayStore::open(&gateway_dir)?,
    );
    let manifest = admin_revision_manifest();
    let policy = PolicyEngine::new(manifest.clone());

    match command {
        AgentRevisionCommands::Create {
            agent_id,
            artifact_id,
            base_revision_id,
            summary,
            json,
        } => {
            let args = serde_json::json!({
                "agent_id": agent_id,
                "artifact_id": artifact_id,
                "base_revision_id": base_revision_id,
                "summary": summary,
            });
            let tool = autonoetic_gateway::runtime::tools::AgentRevisionCreateTool;
            let output = tool.execute(
                &manifest,
                &policy,
                Path::new("/tmp"),
                Some(&gateway_dir),
                &args.to_string(),
                None,
                None,
                Some(&config),
                Some(store.clone()),
                None,
            )?;
            if *json {
                println!("{}", output);
            } else {
                let parsed: serde_json::Value = serde_json::from_str(&output)?;
                println!(
                    "Created revision {} for {}",
                    parsed["revision_id"].as_str().unwrap_or("<unknown>"),
                    parsed["agent_id"].as_str().unwrap_or(agent_id)
                );
            }
        }
        AgentRevisionCommands::Promote {
            agent_id,
            revision_id,
            reason,
            required_eval_run_id,
            json,
        } => {
            let args = serde_json::json!({
                "agent_id": agent_id,
                "revision_id": revision_id,
                "reason": reason,
                "required_eval_run_id": required_eval_run_id,
            });
            let tool = autonoetic_gateway::runtime::tools::AgentRevisionPromoteTool;
            let output = tool.execute(
                &manifest,
                &policy,
                Path::new("/tmp"),
                Some(&gateway_dir),
                &args.to_string(),
                None,
                None,
                Some(&config),
                Some(store.clone()),
                None,
            )?;
            if *json {
                println!("{}", output);
            } else {
                let parsed: serde_json::Value = serde_json::from_str(&output)?;
                println!(
                    "Promoted {} to {}",
                    parsed["agent_id"].as_str().unwrap_or(agent_id),
                    parsed["revision_id"].as_str().unwrap_or(revision_id)
                );
            }
        }
    }
    Ok(())
}

pub fn handle_agent_alias(config_path: &Path, command: &AgentAliasCommands) -> anyhow::Result<()> {
    let config = autonoetic_gateway::config::load_config(config_path)?;
    let gateway_dir = autonoetic_gateway::execution::gateway_root_dir(&config);
    let store = autonoetic_gateway::scheduler::gateway_store::GatewayStore::open(&gateway_dir)?;

    match command {
        AgentAliasCommands::List { agent_id, json } => {
            let aliases = store.list_agent_aliases(agent_id.as_deref())?;
            if *json {
                let mut rows = Vec::new();
                for alias in aliases {
                    let revision =
                        store
                            .get_agent_revision(&alias.revision_id)?
                            .ok_or_else(|| {
                                anyhow::anyhow!(
                                    "Alias '{}' points to missing revision '{}'",
                                    alias.alias_id,
                                    alias.revision_id
                                )
                            })?;
                    rows.push(serde_json::json!({
                        "alias_id": alias.alias_id,
                        "agent_id": alias.agent_id,
                        "revision_id": alias.revision_id,
                        "revision_short_id": revision.short_id,
                        "revision_status": format!("{:?}", revision.status),
                        "updated_at": alias.updated_at,
                        "updated_by_type": alias.updated_by_type,
                        "updated_by_id": alias.updated_by_id,
                        "reason": alias.reason,
                    }));
                }
                println!("{}", serde_json::to_string_pretty(&rows)?);
                return Ok(());
            }

            if aliases.is_empty() {
                println!("No aliases found.");
                return Ok(());
            }

            println!(
                "{:<28} {:<28} {:<30} {:<10} UPDATED AT",
                "ALIAS ID", "AGENT ID", "ACTIVE REVISION", "STATUS"
            );
            for alias in aliases {
                let revision = store
                    .get_agent_revision(&alias.revision_id)?
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "Alias '{}' points to missing revision '{}'",
                            alias.alias_id,
                            alias.revision_id
                        )
                    })?;
                let rev_display = if revision.short_id.is_empty() {
                    alias.revision_id.clone()
                } else {
                    format!("rev_{}", revision.short_id)
                };
                println!(
                    "{:<28} {:<28} {:<30} {:<10} {}",
                    alias.alias_id,
                    alias.agent_id,
                    rev_display,
                    format!("{:?}", revision.status),
                    alias.updated_at
                );
            }
        }
        AgentAliasCommands::Inspect { alias_id, json } => {
            let alias = store.resolve_alias(alias_id)?.ok_or_else(|| {
                anyhow::anyhow!("Alias '{}' not found. Promote a revision first.", alias_id)
            })?;
            let revision = store
                .get_agent_revision(&alias.revision_id)?
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "Alias '{}' points to missing revision '{}'",
                        alias.alias_id,
                        alias.revision_id
                    )
                })?;
            let payload = serde_json::json!({
                "alias_id": alias.alias_id,
                "agent_id": alias.agent_id,
                "active_revision_id": alias.revision_id,
                "active_revision_short_id": revision.short_id,
                "active_revision_status": format!("{:?}", revision.status),
                "updated_at": alias.updated_at,
                "updated_by_type": alias.updated_by_type,
                "updated_by_id": alias.updated_by_id,
                "reason": alias.reason,
            });

            if *json {
                println!("{}", serde_json::to_string_pretty(&payload)?);
            } else {
                println!("alias_id: {}", payload["alias_id"].as_str().unwrap_or(""));
                println!("agent_id: {}", payload["agent_id"].as_str().unwrap_or(""));
                println!(
                    "active_revision: {} (short: rev_{})",
                    payload["active_revision_id"].as_str().unwrap_or(""),
                    payload["active_revision_short_id"].as_str().unwrap_or("")
                );
                println!(
                    "status: {}",
                    payload["active_revision_status"].as_str().unwrap_or("")
                );
                println!(
                    "updated_at: {}",
                    payload["updated_at"].as_str().unwrap_or("")
                );
                println!(
                    "updated_by: {}:{}",
                    payload["updated_by_type"].as_str().unwrap_or(""),
                    payload["updated_by_id"].as_str().unwrap_or("")
                );
                if !payload["reason"].is_null() {
                    println!("reason: {}", payload["reason"].as_str().unwrap_or(""));
                }
            }
        }
        AgentAliasCommands::Suspend {
            alias_id,
            reason,
            by,
            json,
        } => {
            // Confirm the alias exists for a clearer error than a silent no-op.
            if store.resolve_alias(alias_id)?.is_none() {
                anyhow::bail!("Alias '{}' not found. Promote a revision first.", alias_id);
            }
            let changed = store.suspend_agent(alias_id, by, reason.as_deref())?;
            if *json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "alias_id": alias_id,
                        "suspended": changed,
                    }))?
                );
            } else if changed {
                println!(
                    "Agent '{}' suspended. In-flight sessions keep running; no new session can start until unsuspended or re-promoted.",
                    alias_id
                );
            } else {
                println!("Agent '{}' was already suspended; no change.", alias_id);
            }
        }
        AgentAliasCommands::Unsuspend { alias_id, json } => {
            if store.resolve_alias(alias_id)?.is_none() {
                anyhow::bail!("Alias '{}' not found. Promote a revision first.", alias_id);
            }
            let changed = store.unsuspend_agent(alias_id)?;
            if *json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "alias_id": alias_id,
                        "unsuspended": changed,
                    }))?
                );
            } else if changed {
                println!("Agent '{}' unsuspended; it can start new sessions again.", alias_id);
            } else {
                println!("Agent '{}' was not suspended; no change.", alias_id);
            }
        }
    }
    Ok(())
}

pub fn handle_agent_promotion_history(
    config_path: &Path,
    agent_id: Option<&str>,
    json: bool,
) -> anyhow::Result<()> {
    let config = autonoetic_gateway::config::load_config(config_path)?;
    let gateway_dir = autonoetic_gateway::execution::gateway_root_dir(&config);
    let store = autonoetic_gateway::scheduler::gateway_store::GatewayStore::open(&gateway_dir)?;

    let mut rows = Vec::new();
    if let Some(agent_id) = agent_id {
        rows = store.list_promotion_history(agent_id)?;
    } else {
        for alias in store.list_agent_aliases(None)? {
            rows.extend(store.list_promotion_history(&alias.agent_id)?);
        }
        rows.sort_by(|a, b| b.created_at.cmp(&a.created_at));
        rows.dedup_by(|a, b| a.promotion_id == b.promotion_id);
    }

    if json {
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(());
    }

    if rows.is_empty() {
        println!("No promotion history found.");
        return Ok(());
    }

    println!(
        "{:<14} {:<10} {:<28} {:<28} {:<22} CREATED AT",
        "ACTION", "AGENT", "FROM", "TO", "EVAL RUN"
    );
    for record in rows {
        let action = match record.kind {
            PromotionKind::Promote => "promote",
            PromotionKind::Rollback => "rollback",
        };
        println!(
            "{:<14} {:<10} {:<28} {:<28} {:<22} {}",
            action,
            record.agent_id,
            record
                .previous_revision_id
                .unwrap_or_else(|| "-".to_string()),
            record.new_revision_id,
            record.source_eval_run_id.unwrap_or_else(|| "-".to_string()),
            record.created_at
        );
    }
    Ok(())
}

pub fn handle_agent_seed(
    config_path: &Path,
    agent_id: &str,
    revision_id: &str,
    promotion_id: Option<&str>,
    reason: Option<&str>,
    json: bool,
) -> anyhow::Result<()> {
    anyhow::ensure!(!agent_id.trim().is_empty(), "agent_id must not be empty");
    anyhow::ensure!(
        !revision_id.trim().is_empty(),
        "revision_id must not be empty"
    );

    let config = autonoetic_gateway::config::load_config(config_path)?;
    let gateway_dir = autonoetic_gateway::execution::gateway_root_dir(&config);
    let store = autonoetic_gateway::scheduler::gateway_store::GatewayStore::open(&gateway_dir)?;

    let promotion_id = promotion_id.map(|id| id.to_string()).unwrap_or_else(|| {
        let now_nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        autonoetic_types::id_format::mint_hashed_prefixed_id(
            "prom-",
            &format!("{}-{}-{}", agent_id, revision_id, now_nanos),
        )
    });

    let previous_revision_id = store.atomic_promote(
        agent_id,
        revision_id,
        &promotion_id,
        "human",
        "cli.seed",
        reason,
        None,
        None,
    )?;
    let payload = serde_json::json!({
        "ok": true,
        "agent_id": agent_id,
        "revision_id": revision_id,
        "promotion_id": promotion_id,
        "previous_revision_id": previous_revision_id,
    });

    if json {
        println!("{}", serde_json::to_string_pretty(&payload)?);
    } else {
        println!(
            "Seeded alias '{}' to revision '{}' (promotion_id: {})",
            agent_id,
            revision_id,
            payload["promotion_id"].as_str().unwrap_or("")
        );
        if let Some(prev) = payload["previous_revision_id"].as_str() {
            println!("Previous revision: {}", prev);
        }
    }
    Ok(())
}

pub fn handle_agent_bootstrap(
    config_path: &Path,
    from: Option<&str>,
    overwrite: bool,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        config_path.exists(),
        "Config file not found at {}. Create it first (or pass a valid --config path) before running 'agent bootstrap'.",
        config_path.display()
    );
    let config = autonoetic_gateway::config::load_config(config_path)?;
    let install = install_reference_agents(&config, from, overwrite)?;

    if autonoetic_gateway::bootstrap::ensure_vault_key_for_bootstrap_workspace(&config)? {
        println!(
            "Created vault master key at {} — back it up; without it encrypted credentials cannot be decrypted.",
            autonoetic_gateway::execution::gateway_root_dir(&config)
                .join("vault.key")
                .display()
        );
    }

    let gateway_dir = autonoetic_gateway::execution::gateway_root_dir(&config);
    let activated = autonoetic_gateway::bootstrap_agents(&config, &gateway_dir)?;

    println!(
        "Bootstrap complete: {} installed, {} overwritten, {} skipped, {} activated (target: {}).",
        install.copied,
        install.overwritten,
        install.skipped,
        activated,
        config.agents_dir.display()
    );

    Ok(())
}

/// Result of copying reference agent bundles into `config.agents_dir`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct InstallReferenceAgentsResult {
    pub copied: usize,
    pub overwritten: usize,
    pub skipped: usize,
}

/// Copy bundled reference agents from the repo (or `--from`) into `config.agents_dir`.
/// When `overwrite` is true, existing agent directories are replaced with the
/// latest reference bundles.
pub fn install_reference_agents(
    config: &autonoetic_types::config::GatewayConfig,
    from: Option<&str>,
    overwrite: bool,
) -> anyhow::Result<InstallReferenceAgentsResult> {
    std::fs::create_dir_all(&config.agents_dir)?;

    let reference_root = resolve_reference_agents_dir(from)?;
    let bundles = discover_reference_bundles(&reference_root)?;
    anyhow::ensure!(
        !bundles.is_empty(),
        "No reference bundles found under {}",
        reference_root.display()
    );

    let mut copied = 0_usize;
    let mut overwritten = 0_usize;
    let mut skipped = 0_usize;

    for bundle in bundles {
        let agent_id = bundle
            .file_name()
            .and_then(|value| value.to_str())
            .ok_or_else(|| {
                anyhow::anyhow!("Invalid bundle directory name: {}", bundle.display())
            })?;
        let target_dir = config.agents_dir.join(agent_id);
        if target_dir.exists() {
            if overwrite {
                std::fs::remove_dir_all(&target_dir)?;
                copy_dir_recursive(&bundle, &target_dir)?;
                overwritten += 1;
                println!("Overwrote '{}' from {}", agent_id, bundle.display());
            } else {
                skipped += 1;
                println!(
                    "Skipped '{}' (already exists at {})",
                    agent_id,
                    target_dir.display()
                );
            }
            continue;
        }
        copy_dir_recursive(&bundle, &target_dir)?;
        copied += 1;
        println!("Installed '{}' from {}", agent_id, bundle.display());
    }

    Ok(InstallReferenceAgentsResult {
        copied,
        overwritten,
        skipped,
    })
}

fn resolve_reference_agents_dir(from: Option<&str>) -> anyhow::Result<std::path::PathBuf> {
    if let Some(path) = from {
        let explicit = std::path::PathBuf::from(path);
        anyhow::ensure!(
            explicit.is_dir(),
            "Provided --from path is not a directory: {}",
            explicit.display()
        );
        return Ok(explicit);
    }

    if let Ok(path) = std::env::var("AUTONOETIC_REFERENCE_AGENTS_DIR") {
        let env_path = std::path::PathBuf::from(path);
        if env_path.is_dir() {
            return Ok(env_path);
        }
    }

    let cargo_manifest = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let mut candidates = vec![
        // From autonoetic/autonoetic/ -> ../../agents (workspace root agents/)
        cargo_manifest.join("../../agents"),
        // From autonoetic/autonoetic/ -> ../agents (autonoetic/agents)
        cargo_manifest.join("../agents"),
        // From autonoetic/autonoetic/ -> ../autonoetic/agents (same as above, explicit)
        cargo_manifest
            .parent()
            .and_then(|p| p.parent())
            .map(|p| p.join("agents"))
            .unwrap_or_default(),
    ];
    if let Ok(cwd) = std::env::current_dir() {
        candidates.push(cwd.join("agents"));
        candidates.push(cwd.join("../agents"));
        candidates.push(cwd.join("../../agents"));
    }

    for candidate in candidates.iter().filter(|c| !c.as_os_str().is_empty()) {
        if candidate.is_dir() {
            return Ok(candidate.clone());
        }
    }

    anyhow::bail!(
        "Could not auto-detect reference bundles directory. Provide --from <path> or set AUTONOETIC_REFERENCE_AGENTS_DIR.\n\
        Searched: {:?}",
        candidates.iter().map(|c| c.display().to_string()).collect::<Vec<_>>()
    )
}

fn discover_reference_bundles(root: &Path) -> anyhow::Result<Vec<std::path::PathBuf>> {
    let mut bundles = Vec::new();
    for group in std::fs::read_dir(root)? {
        let group = group?;
        if !group.file_type()?.is_dir() {
            continue;
        }
        let group_path = group.path();
        if group_path.join("SKILL.md").exists() {
            // Top-level bundle (e.g. agents/autonoetic.digest/) — not nested under a role group.
            bundles.push(group_path);
            continue;
        }
        for entry in std::fs::read_dir(group_path)? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let bundle_dir = entry.path();
            if bundle_dir.join("SKILL.md").exists() {
                bundles.push(bundle_dir);
            }
        }
    }
    bundles.sort();
    Ok(bundles)
}

fn copy_dir_recursive(src: &Path, dst: &Path) -> anyhow::Result<()> {
    anyhow::ensure!(src.is_dir(), "Source is not a directory: {}", src.display());
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir_recursive(&src_path, &dst_path)?;
        } else {
            std::fs::copy(&src_path, &dst_path)?;
        }
    }
    Ok(())
}

/// Pretty-print per-completion token stats (and optional % of declared context window).
pub fn format_llm_usage_for_cli(usages: &[LlmExchangeUsage]) -> Option<String> {
    if usages.is_empty() {
        return None;
    }
    let total_in: u64 = usages.iter().map(|u| u.input_tokens).sum();
    let total_out: u64 = usages.iter().map(|u| u.output_tokens).sum();
    let mut lines = vec![format!(
        "[LLM usage] {} completion(s) · tokens in={} · out={}",
        usages.len(),
        total_in,
        total_out
    )];
    for (i, u) in usages.iter().enumerate() {
        let mut s = format!(
            "  · #{} {} · in={} · out={}",
            i + 1,
            u.model,
            u.input_tokens,
            u.output_tokens
        );
        if let (Some(p), Some(w)) = (u.input_context_pct, u.context_window_tokens) {
            s.push_str(&format!(" · prompt ~{:.1}% of {} ctx", p, w));
        }
        if let Some(usd) = u.estimated_cost_usd {
            s.push_str(&format!(" · ~${:.6} (est.)", usd));
        }
        lines.push(s);
    }
    Some(lines.join("\n"))
}

pub async fn handle_agent_run(
    config_path: &Path,
    agent_id: &str,
    message: Option<&str>,
    interactive: bool,
    headless: bool,
    response_validation: Option<super::common::ResponseValidationMode>,
    record_network: bool,
    recording_duration: Option<u64>,
    recording_max_requests: Option<u64>,
    recording_max_bytes: Option<u64>,
) -> anyhow::Result<()> {
    info!(
        "Running Agent {} (interactive: {}, headless: {}, record_network: {})",
        agent_id, interactive, headless, record_network
    );
    if let Some(msg) = message {
        info!("Kickoff message: {}", msg);
    }
    run_agent_with_runtime(
        config_path,
        agent_id,
        message,
        interactive,
        headless,
        response_validation,
        record_network,
        recording_duration,
        recording_max_requests,
        recording_max_bytes,
    )
    .await
}

pub async fn run_agent_with_runtime(
    config_path: &Path,
    agent_id: &str,
    kickoff_message: Option<&str>,
    interactive: bool,
    headless: bool,
    response_validation: Option<super::common::ResponseValidationMode>,
    record_network: bool,
    recording_duration: Option<u64>,
    recording_max_requests: Option<u64>,
    recording_max_bytes: Option<u64>,
) -> anyhow::Result<()> {
    let mut loaded_config = autonoetic_gateway::config::load_config(config_path)?;
    super::common::apply_response_validation_override(&mut loaded_config, response_validation);

    if record_network {
        let gateway_dir = loaded_config.agents_dir.join(".gateway");
        std::fs::create_dir_all(&gateway_dir)?;
        let store = Arc::new(
            autonoetic_gateway::scheduler::gateway_store::GatewayStore::open(&gateway_dir)?,
        );

        let session_id = format!("rs_{:x}", uuid::Uuid::new_v4().as_u128());
        let staging_dir = gateway_dir.join("recordings").join(&session_id).join("fixtures");
        std::fs::create_dir_all(&staging_dir)?;

        let recording_session = autonoetic_types::recording::RecordingSession {
            session_id,
            agent_id: agent_id.to_string(),
            artifact_id: String::new(),
            revision_id: String::new(),
            root_session_id: format!("root-{}", uuid::Uuid::new_v4().as_simple()),
            started_at: chrono::Utc::now().to_rfc3339(),
            stopped_at: None,
            duration_secs: recording_duration.map(|d| d as i64),
            max_requests: recording_max_requests.map(|r| r as i64),
            max_bytes: recording_max_bytes.map(|b| b as i64),
            request_count: 0,
            total_bytes: 0,
            status: autonoetic_types::recording::RecordingStatus::Active,
            fixture_set_id: None,
            created_by: "cli".to_string(),
        };
        store.create_recording_session(&recording_session)?;

        // Emit causal event for recording session start.
        let causal_event = autonoetic_types::causal_chain::CausalEventRecord {
            event_id: uuid::Uuid::new_v4().to_string(),
            agent_id: agent_id.to_string(),
            session_id: recording_session.session_id.clone(),
            turn_id: None,
            event_seq: chrono::Utc::now().timestamp_millis().max(0) as u64,
            timestamp: chrono::Utc::now().to_rfc3339(),
            category: "artifact".to_string(),
            action: "artifact.fixture_recording_session_started".to_string(),
            status: "active".to_string(),
            enforced_rules: vec![],
            target: Some(agent_id.to_string()),
            payload: Some(
                serde_json::json!({
                    "recording_session_id": &recording_session.session_id,
                    "staging_dir": staging_dir.to_string_lossy(),
                    "duration_secs": recording_duration,
                    "max_requests": recording_max_requests,
                    "max_bytes": recording_max_bytes,
                })
                .to_string(),
            ),
            payload_ref: None,
            evidence_ref: None,
            reason: None,
        };
        let _ = store.create_causal_event(&causal_event);

        eprintln!(
            "  Recording session {} started. Fixtures will be written to: {}",
            recording_session.session_id,
            staging_dir.display()
        );
    }

    let gateway_config = Arc::new(loaded_config);
    let repo = autonoetic_gateway::AgentRepository::from_config(&gateway_config);
    let loaded = repo.get_sync(agent_id)?;
    let manifest = loaded.manifest;
    let instructions = loaded.instructions;
    let agent_dir = loaded.dir;

    // Override sandbox_network to Recording when --record-network is active.
    let manifest = if record_network {
        autonoetic_types::agent::AgentManifest {
            sandbox_network: autonoetic_types::agent::SandboxNetworkPolicy::Recording,
            ..manifest
        }
    } else {
        manifest
    };

    let profile = autonoetic_gateway::runtime::inference_profile::resolve_inference_profile(
        agent_id,
        &manifest,
        &gateway_config,
        None,
    )?;
    let driver =
        autonoetic_gateway::llm::build_driver(profile.llm_config, reqwest::Client::new())?;
    run_agent_with_runtime_with_driver(
        manifest,
        instructions,
        agent_dir,
        kickoff_message,
        interactive,
        headless,
        driver,
        Some(gateway_config),
        None,
    )
    .await
}

#[allow(dead_code)]
pub fn load_agent_runtime_context(
    config_path: &Path,
    agent_id: &str,
) -> anyhow::Result<(
    autonoetic_types::agent::AgentManifest,
    String,
    std::path::PathBuf,
)> {
    let config = autonoetic_gateway::config::load_config(config_path)?;
    let repo = autonoetic_gateway::AgentRepository::from_config(&config);
    let loaded = repo.get_sync(agent_id)?;
    Ok((loaded.manifest, loaded.instructions, loaded.dir))
}

fn session_close_outcome_from_headless_turn_outcome(
    outcome: &autonoetic_gateway::runtime::lifecycle::TurnOutcome,
) -> SessionCloseOutcome {
    use autonoetic_gateway::runtime::lifecycle::TurnOutcome;
    match outcome {
        TurnOutcome::Completed(Some(_)) => SessionCloseOutcome::HeadlessComplete,
        TurnOutcome::Completed(None) => SessionCloseOutcome::HeadlessCompleteEmpty,
        TurnOutcome::Suspended { .. } => SessionCloseOutcome::HeadlessSuspended,
        TurnOutcome::SuspendedUserInput { .. } => {
            SessionCloseOutcome::HeadlessSuspendedUserInput
        }
        TurnOutcome::Escalated { .. } => SessionCloseOutcome::HeadlessEscalated,
    }
}

pub async fn run_agent_with_runtime_with_driver(
    manifest: autonoetic_types::agent::AgentManifest,
    instructions: String,
    agent_dir: std::path::PathBuf,
    kickoff_message: Option<&str>,
    interactive: bool,
    headless: bool,
    driver: Arc<dyn autonoetic_gateway::llm::LlmDriver>,
    gateway_config: Option<Arc<GatewayConfig>>,
    session_budget: Option<Arc<autonoetic_gateway::SessionBudgetRegistry>>,
) -> anyhow::Result<()> {
    if headless {
        tracing::info!("Headless mode enabled.");
    }

    let mut runtime = autonoetic_gateway::runtime::lifecycle::AgentExecutor::new(
        manifest,
        instructions,
        driver,
        agent_dir,
        autonoetic_gateway::runtime::tools::default_registry(),
        None,
    );
    if let Some(cfg) = gateway_config.as_ref() {
        runtime = runtime
            .with_gateway_dir(cfg.agents_dir.join(".gateway"))
            .with_config(cfg.clone());
    }
    if let Some(budget) = session_budget {
        runtime = runtime.with_session_budget(Some(budget));
    } else if let Some(cfg) = gateway_config.as_ref() {
        let b = Arc::new(autonoetic_gateway::SessionBudgetRegistry::new(
            cfg.session_budget.clone(),
        ));
        runtime = runtime.with_session_budget(Some(b));
    }
    let or_catalog = Arc::new(autonoetic_gateway::OpenRouterCatalog::new(
        reqwest::Client::new(),
    ));
    runtime = runtime.with_openrouter_catalog(Some(or_catalog));
    if let Some(message) = kickoff_message {
        runtime = runtime.with_initial_user_message(message.to_string());
    }
    if interactive {
        return run_interactive_session(&mut runtime, kickoff_message).await;
    }

    let mut history = vec![
        Message::system(runtime.instructions.clone()),
        Message::user(runtime.initial_user_message.clone()),
    ];
    use autonoetic_gateway::runtime::lifecycle::TurnOutcome;
    match runtime.execute_with_history(&mut history).await {
        Ok(outcome) => {
            match &outcome {
                TurnOutcome::Completed(Some(reply)) => {
                    println!("{}", reply);
                    if let Some(u) = format_llm_usage_for_cli(&runtime.take_llm_usage_last_run()) {
                        eprintln!("{}", u);
                    }
                }
                TurnOutcome::Completed(None) => {
                    println!("[No assistant text returned]");
                    if let Some(u) = format_llm_usage_for_cli(&runtime.take_llm_usage_last_run()) {
                        eprintln!("{}", u);
                    }
                }
                TurnOutcome::Suspended {
                    approval_request_id,
                    ..
                } => {
                    println!("[Turn suspended pending approval: {}]", approval_request_id);
                }
                TurnOutcome::SuspendedUserInput { interaction_id } => {
                    println!(
                        "[Turn suspended pending user interaction: {}]",
                        interaction_id
                    );
                }
                TurnOutcome::Escalated { escalation_request_id } => {
                    println!(
                        "[Turn suspended pending human escalation: {}]",
                        escalation_request_id
                    );
                }
            }
            runtime.close_session(
                session_close_outcome_from_headless_turn_outcome(&outcome),
            )?;
        }
        Err(e) => {
            let _ = runtime.close_session(SessionCloseOutcome::HeadlessError);
            return Err(e);
        }
    }
    Ok(())
}

pub async fn run_interactive_session(
    runtime: &mut autonoetic_gateway::runtime::lifecycle::AgentExecutor,
    kickoff_message: Option<&str>,
) -> anyhow::Result<()> {
    use autonoetic_gateway::runtime::lifecycle::TurnOutcome;
    let mut stdout = tokio::io::stdout();
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let mut history = vec![Message::system(runtime.instructions.clone())];

    stdout
        .write_all(b"Interactive mode enabled. Type /exit to quit.\n")
        .await?;
    stdout.flush().await?;

    if let Some(message) = kickoff_message {
        history.push(Message::user(message.to_string()));
        match runtime.execute_with_history(&mut history).await {
            Ok(TurnOutcome::Completed(Some(reply))) => {
                stdout.write_all(reply.as_bytes()).await?;
                stdout.write_all(b"\n").await?;
                stdout.flush().await?;
                if let Some(u) = format_llm_usage_for_cli(&runtime.take_llm_usage_last_run()) {
                    eprintln!("{}", u);
                }
            }
            Ok(TurnOutcome::Completed(None)) => {
                if let Some(u) = format_llm_usage_for_cli(&runtime.take_llm_usage_last_run()) {
                    eprintln!("{}", u);
                }
            }
            Ok(TurnOutcome::Suspended {
                approval_request_id,
                ..
            }) => {
                stdout
                    .write_all(
                        format!(
                            "[Turn suspended pending approval: {}]\n",
                            approval_request_id
                        )
                        .as_bytes(),
                    )
                    .await?;
                stdout.flush().await?;
            }
            Ok(TurnOutcome::SuspendedUserInput { interaction_id }) => {
                stdout
                    .write_all(
                        format!(
                            "[Turn suspended pending user interaction: {}]\n",
                            interaction_id
                        )
                        .as_bytes(),
                    )
                    .await?;
                stdout.flush().await?;
            }
            Ok(TurnOutcome::Escalated { escalation_request_id }) => {
                stdout
                    .write_all(
                        format!(
                            "[Turn suspended pending human escalation: {}]\n",
                            escalation_request_id
                        )
                        .as_bytes(),
                    )
                    .await?;
                stdout.flush().await?;
            }
            Err(e) => {
                let _ = runtime.close_session(SessionCloseOutcome::InteractiveError);
                return Err(e);
            }
        };
    }

    loop {
        stdout.write_all(b"> ").await?;
        stdout.flush().await?;

        let Some(line) = lines.next_line().await? else {
            break;
        };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if trimmed == "/exit" || trimmed == "/quit" {
            break;
        }

        history.push(Message::user(trimmed.to_string()));
        match runtime.execute_with_history(&mut history).await {
            Ok(TurnOutcome::Completed(Some(reply))) => {
                stdout.write_all(reply.as_bytes()).await?;
                stdout.write_all(b"\n").await?;
                stdout.flush().await?;
                if let Some(u) = format_llm_usage_for_cli(&runtime.take_llm_usage_last_run()) {
                    eprintln!("{}", u);
                }
            }
            Ok(TurnOutcome::Completed(None)) => {
                if let Some(u) = format_llm_usage_for_cli(&runtime.take_llm_usage_last_run()) {
                    eprintln!("{}", u);
                }
            }
            Ok(TurnOutcome::Suspended {
                approval_request_id,
                ..
            }) => {
                stdout
                    .write_all(
                        format!(
                            "[Turn suspended pending approval: {}]\n",
                            approval_request_id
                        )
                        .as_bytes(),
                    )
                    .await?;
                stdout.flush().await?;
            }
            Ok(TurnOutcome::SuspendedUserInput { interaction_id }) => {
                stdout
                    .write_all(
                        format!(
                            "[Turn suspended pending user interaction: {}]\n",
                            interaction_id
                        )
                        .as_bytes(),
                    )
                    .await?;
                stdout.flush().await?;
            }
            Ok(TurnOutcome::Escalated { escalation_request_id }) => {
                stdout
                    .write_all(
                        format!(
                            "[Turn suspended pending human escalation: {}]\n",
                            escalation_request_id
                        )
                        .as_bytes(),
                    )
                    .await?;
                stdout.flush().await?;
            }
            Err(e) => {
                let _ = runtime.close_session(SessionCloseOutcome::InteractiveError);
                return Err(e);
            }
        };
    }
    runtime.close_session(SessionCloseOutcome::InteractiveExit)?;
    Ok(())
}

pub fn handle_agent_import_skill(
    config_path: &Path,
    from: &str,
    agent_id: &str,
    trust: crate::cli::common::TrustMode,
    provider: Option<&str>,
    model: Option<&str>,
) -> anyhow::Result<()> {
    use crate::cli::common::TrustMode;
    use autonoetic_types::agent::AgentSkillsImportMetadata;

    let skill_dir = std::path::Path::new(from);
    anyhow::ensure!(
        skill_dir.exists(),
        "Skill directory does not exist: {}",
        from
    );

    let skill_manifest_path = skill_dir.join("SKILL.md");
    anyhow::ensure!(
        skill_manifest_path.exists(),
        "SKILL.md not found in: {}",
        from
    );

    let skill_content = std::fs::read_to_string(&skill_manifest_path)?;
    let (parsed_manifest, body) =
        autonoetic_gateway::runtime::parser::SkillParser::parse(&skill_content)?;

    let import_license: Option<String> = parsed_manifest
        .agentskills_import
        .as_ref()
        .and_then(|ai| ai.license.clone());
    let import_compatibility: Option<String> = parsed_manifest
        .agentskills_import
        .as_ref()
        .and_then(|ai| ai.compatibility.clone());
    let import_allowed_tools: Vec<String> = parsed_manifest
        .agentskills_import
        .as_ref()
        .map(|ai| ai.allowed_tools.clone())
        .unwrap_or_default();

    let agentskills_import = if !import_allowed_tools.is_empty()
        || import_license.is_some()
        || import_compatibility.is_some()
    {
        Some(AgentSkillsImportMetadata {
            license: import_license.clone(),
            compatibility: import_compatibility.clone(),
            allowed_tools: import_allowed_tools.clone(),
            needs_tool_bridging: !import_allowed_tools.is_empty(),
        })
    } else {
        parsed_manifest.agentskills_import.clone()
    };

    info!(
        "Importing AgentSkills skill '{}' as '{}'",
        parsed_manifest.agent.id, agent_id
    );

    let config = autonoetic_gateway::config::load_config(config_path)?;
    let llm_config = if let Some(p) = provider {
        Some(autonoetic_types::agent::LlmConfig {
            provider: p.to_string(),
            model: model.unwrap_or("gpt-4o").to_string(),
            temperature: 0.2,
            fallback_provider: None,
            fallback_model: None,
            chat_only: false,
            context_window_tokens: None,
            base_url: None,
            api_key_env: None,
            routing_preset: None,
            thinking: None,
        })
    } else {
        let resolved = resolve_llm_config(&config, None, None, provider, model);
        Some(autonoetic_types::agent::LlmConfig {
            provider: resolved.provider,
            model: resolved.model,
            temperature: resolved.temperature,
            fallback_provider: None,
            fallback_model: None,
            chat_only: resolved.chat_only,
            context_window_tokens: None,
            base_url: resolved.base_url,
            api_key_env: resolved.api_key_env,
            routing_preset: resolved.routing_preset,
            thinking: None,
        })
    };

    let (capabilities, trust_applied) = match trust {
        TrustMode::Generous => {
            let caps = if parsed_manifest.capabilities.is_empty() {
                let allowed_tools: Vec<String> = parsed_manifest
                    .agentskills_import
                    .as_ref()
                    .map(|m| m.allowed_tools.clone())
                    .unwrap_or_default();
                if allowed_tools.is_empty() {
                    vec![
                        autonoetic_types::capability::Capability::ReadAccess {
                            scopes: vec!["self.*".to_string()],
                        },
                        autonoetic_types::capability::Capability::WriteAccess {
                            scopes: vec!["self.*".to_string()],
                        },
                        autonoetic_types::capability::Capability::CodeExecution {
                            patterns: vec!["*".to_string()],
                            commands: vec![],
                        },
                    ]
                } else {
                    autonoetic_gateway::runtime::parser::infer_capabilities(&allowed_tools)
                }
            } else {
                parsed_manifest.capabilities.clone()
            };
            (caps, false)
        }
        TrustMode::Strict => {
            let mut caps = if parsed_manifest.capabilities.is_empty() {
                let allowed_tools: Vec<String> = parsed_manifest
                    .agentskills_import
                    .as_ref()
                    .map(|m| m.allowed_tools.clone())
                    .unwrap_or_default();
                if allowed_tools.is_empty() {
                    vec![autonoetic_types::capability::Capability::ReadAccess {
                        scopes: vec!["self.*".to_string()],
                    }]
                } else {
                    autonoetic_gateway::runtime::parser::infer_capabilities(&allowed_tools)
                }
            } else {
                parsed_manifest.capabilities.clone()
            };
            caps.push(autonoetic_types::capability::Capability::ApprovalQueue {
                patterns: vec!["*".to_string()],
            });
            (caps, true)
        }
        TrustMode::Audit => {
            let mut caps = vec![autonoetic_types::capability::Capability::ReadAccess {
                scopes: vec!["self.*".to_string()],
            }];
            caps.push(autonoetic_types::capability::Capability::ApprovalQueue {
                patterns: vec!["*".to_string()],
            });
            (caps, true)
        }
    };

    let target_manifest = autonoetic_types::agent::AgentManifest {
        version: parsed_manifest.version.clone(),
        runtime: parsed_manifest.runtime.clone(),
        agent: autonoetic_types::agent::AgentIdentity {
            id: agent_id.to_string(),
            name: parsed_manifest.agent.name.clone(),
            description: parsed_manifest.agent.description.clone(),
        },
        capabilities,
        llm_preset: parsed_manifest.llm_preset.clone(),
        llm_overrides: parsed_manifest.llm_overrides.clone(),
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
        allowed_tool_tiers: parsed_manifest.allowed_tool_tiers.clone(),
        agentskills_import,
        compression: parsed_manifest.compression.clone(),
            open_web: false,
        sandbox_network: parsed_manifest.sandbox_network,
    };

    let agents_dir = config_path
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."))
        .join("agents");
    let target_dir = agents_dir.join(agent_id.replace('.', "-"));
    std::fs::create_dir_all(&target_dir)?;

    let yaml_frontmatter = {
        let mut lines = Vec::new();
        lines.push(format!("name: \"{}\"", agent_id));
        lines.push(format!(
            "description: \"{}\"",
            target_manifest.agent.description
        ));
        if let Some(ref l) = import_license {
            lines.push(format!("license: \"{}\"", l));
        }
        if let Some(ref c) = import_compatibility {
            lines.push(format!("compatibility: \"{}\"", c));
        }
        if !import_allowed_tools.is_empty() {
            lines.push("allowed-tools:".to_string());
            for t in &import_allowed_tools {
                lines.push(format!("  - \"{}\"", t));
            }
        }
        lines.push("metadata:".to_string());
        lines.push("  autonoetic:".to_string());
        lines.push(format!("    version: \"{}\"", target_manifest.version));
        lines.push("    runtime:".to_string());
        lines.push(format!(
            "      engine: \"{}\"",
            target_manifest.runtime.engine
        ));
        lines.push(format!(
            "      gateway_version: \"{}\"",
            target_manifest.runtime.gateway_version
        ));
        lines.push(format!(
            "      sdk_version: \"{}\"",
            target_manifest.runtime.sdk_version
        ));
        lines.push(format!(
            "      type: \"{}\"",
            target_manifest.runtime.runtime_type
        ));
        lines.push(format!(
            "      sandbox: \"{}\"",
            target_manifest.runtime.sandbox
        ));
        lines.push(format!(
            "      runtime_lock: \"{}\"",
            target_manifest.runtime.runtime_lock
        ));
        lines.push("    agent:".to_string());
        lines.push(format!("      id: \"{}\"", target_manifest.agent.id));
        lines.push(format!("      name: \"{}\"", target_manifest.agent.name));
        lines.push(format!(
            "      description: \"{}\"",
            target_manifest.agent.description
        ));
        if !target_manifest.capabilities.is_empty() {
            lines.push("    capabilities:".to_string());
            for cap in &target_manifest.capabilities {
                let cap_yaml = serde_json::to_string(cap).unwrap_or_default();
                lines.push(format!("      - {}", cap_yaml));
            }
        }
        if let Some(ref preset) = target_manifest.llm_preset {
            lines.push(format!("    llm_preset: {}", preset));
        }
        if let Some(ref overrides) = target_manifest.llm_overrides {
            if let Ok(yaml) = serde_yaml::to_string(overrides) {
                let yaml = yaml.strip_prefix("---\n").unwrap_or(&yaml);
                if !yaml.trim().is_empty() {
                    lines.push("    llm_overrides:".to_string());
                    for line in yaml.lines() {
                        lines.push(format!("      {}", line));
                    }
                }
            }
        }
        if let Some(ref llm) = target_manifest.llm_config {
            lines.push("    llm_config:".to_string());
            lines.push(format!("      provider: \"{}\"", llm.provider));
            lines.push(format!("      model: \"{}\"", llm.model));
            lines.push(format!("      temperature: {}", llm.temperature));
            if let Some(ref fb) = llm.fallback_provider {
                lines.push(format!("      fallback_provider: \"{}\"", fb));
            }
            if let Some(ref fb) = llm.fallback_model {
                lines.push(format!("      fallback_model: \"{}\"", fb));
            }
            lines.push(format!("      chat_only: {}", llm.chat_only));
            if let Some(cw) = llm.context_window_tokens {
                lines.push(format!("      context_window_tokens: {}", cw));
            }
            if let Some(ref bu) = llm.base_url {
                lines.push(format!("      base_url: \"{}\"", bu));
            }
        }
        if let Some(ref limits) = target_manifest.limits {
            lines.push("    limits:".to_string());
            lines.push(format!("      max_memory_mb: {}", limits.max_memory_mb));
            lines.push(format!(
                "      max_execution_time_sec: {}",
                limits.max_execution_time_sec
            ));
            if let Some(tb) = limits.token_budget_monthly {
                lines.push(format!("      token_budget_monthly: {}", tb));
            }
        }
        if let Some(ref bg) = target_manifest.background {
            lines.push("    background:".to_string());
            lines.push(format!("      enabled: {}", bg.enabled));
            lines.push(format!("      interval_secs: {}", bg.interval_secs));
            lines.push(format!(
                "      mode: {}",
                match bg.mode {
                    autonoetic_types::background::BackgroundMode::Deterministic => "deterministic",
                    autonoetic_types::background::BackgroundMode::Reasoning => "reasoning",
                }
            ));
            lines.push("      wake_predicates:".to_string());
            lines.push(format!("        timer: {}", bg.wake_predicates.timer));
            lines.push(format!(
                "        approval_resolved: {}",
                bg.wake_predicates.approval_resolved
            ));
            lines.push(format!(
                "      validate_on_install: {}",
                bg.validate_on_install
            ));
        }
        if let Some(ref disclosure) = target_manifest.disclosure {
            lines.push("    disclosure:".to_string());
            if disclosure.default_class.is_restricted() {
                lines.push(format!("      default_class: restricted"));
            }
            if !disclosure.rules.is_empty() {
                lines.push("      rules:".to_string());
                for rule in &disclosure.rules {
                    let r_yaml = serde_json::to_string(rule).unwrap_or_default();
                    lines.push(format!("        - {}", r_yaml));
                }
            }
        }
        if let Some(ref io) = target_manifest.io {
            lines.push("    io:".to_string());
            if let Some(ref accepts) = io.accepts {
                lines.push(format!(
                    "      accepts: {}",
                    serde_json::to_string(accepts).unwrap_or_default()
                ));
            }
            if let Some(ref returns) = io.returns {
                lines.push(format!(
                    "      returns: {}",
                    serde_json::to_string(returns).unwrap_or_default()
                ));
            }
            if let Some(ref output_policy) = io.output_policy {
                lines.push(format!(
                    "      output_policy: {}",
                    serde_json::to_string(output_policy).unwrap_or_default()
                ));
            }
        }
        if let Some(ref mw) = target_manifest.middleware {
            if mw.pre_process.is_some() || mw.post_process.is_some() {
                lines.push("    middleware:".to_string());
                if let Some(ref p) = mw.pre_process {
                    lines.push(format!("      pre_process: \"{}\"", p));
                }
                if let Some(ref p) = mw.post_process {
                    lines.push(format!("      post_process: \"{}\"", p));
                }
            }
        }
        lines.push(format!(
            "    execution_mode: {}",
            match target_manifest.execution_mode {
                autonoetic_types::agent::ExecutionMode::Script => "script",
                autonoetic_types::agent::ExecutionMode::Reasoning => "reasoning",
            }
        ));
        if let Some(ref se) = target_manifest.script_entry {
            lines.push(format!("    script_entry: \"{}\"", se));
        }
        if !target_manifest.allowed_tool_tiers.is_empty() {
            lines.push("    allowed_tool_tiers:".to_string());
            for tier in &target_manifest.allowed_tool_tiers {
                lines.push(format!(
                    "      - {}",
                    match tier {
                        autonoetic_types::agent::ToolTier::Core => "core",
                        autonoetic_types::agent::ToolTier::Workflow => "workflow",
                        autonoetic_types::agent::ToolTier::Specialized => "specialized",
                    }
                ));
            }
        }
        lines.join("\n")
    };

    let output_skill = format!("---\n{}\n---\n\n{}", yaml_frontmatter, body.trim());
    std::fs::write(target_dir.join("SKILL.md"), &output_skill)?;

    let runtime_lock_content = default_runtime_lock_contents();
    std::fs::write(target_dir.join("runtime.lock"), runtime_lock_content)?;

    for subdir in &["scripts", "references", "assets"] {
        let src = skill_dir.join(subdir);
        let dst = target_dir.join(subdir);
        if src.is_dir() {
            fn copy_dir_recursive(
                src: &std::path::Path,
                dst: &std::path::Path,
            ) -> std::io::Result<()> {
                std::fs::create_dir_all(dst)?;
                for entry in std::fs::read_dir(src)? {
                    let entry = entry?;
                    let path = entry.path();
                    let target = dst.join(entry.file_name());
                    if path.is_dir() {
                        copy_dir_recursive(&path, &target)?;
                    } else {
                        std::fs::copy(&path, &target)?;
                    }
                }
                Ok(())
            }
            copy_dir_recursive(&src, &dst)?;
            info!("Copied {} to {}", subdir, dst.display());
        }
    }

    info!("Imported skill written to: {}", target_dir.display());

    if trust_applied {
        let trust_label = match trust {
            TrustMode::Generous => "generous",
            TrustMode::Strict => "strict",
            TrustMode::Audit => "audit",
        };
        info!(
            "Trust mode '{}' applied: imported agent capabilities are {}.",
            trust_label,
            match trust {
                TrustMode::Generous => "unchanged",
                TrustMode::Strict => "preserved but all privileged operations require approval",
                TrustMode::Audit => "restricted to read-only; all operations require approval",
            }
        );
    }

    if let Some(import_meta) = &target_manifest.agentskills_import {
        if import_meta.needs_tool_bridging {
            info!("Tool bridging will be injected at runtime");
        }
    }

    info!(
        "Agent '{}' imported successfully. Run 'autonoetic agent bootstrap' then 'autonoetic agent revision create/promote' to activate.",
        agent_id
    );

    Ok(())
}

pub fn handle_agent_credential(
    config_path: &Path,
    command: &AgentCredentialCommands,
) -> anyhow::Result<()> {
    match command {
        AgentCredentialCommands::Put {
            service,
            secret_name,
            from_env,
            value,
            credential_id,
            inject_as,
            allowed_hosts,
            expires_at,
        } => handle_credential_put(
            config_path,
            service,
            secret_name,
            from_env,
            value,
            credential_id,
            inject_as,
            allowed_hosts,
            expires_at,
        ),
        AgentCredentialCommands::List { service, json } => {
            handle_credential_list(config_path, service, *json)
        }
        AgentCredentialCommands::Rm { credential_id } => {
            handle_credential_rm(config_path, credential_id)
        }
    }
}

fn handle_credential_put(
    config_path: &Path,
    service: &str,
    secret_name: &str,
    from_env: &Option<String>,
    value: &Option<String>,
    credential_id: &Option<String>,
    inject_as: &Option<String>,
    allowed_hosts: &Option<Vec<String>>,
    expires_at: &Option<String>,
) -> anyhow::Result<()> {
    let config = autonoetic_gateway::config::load_config(config_path)?;
    let gateway_dir = autonoetic_gateway::execution::gateway_root_dir(&config);
    let store = autonoetic_gateway::scheduler::gateway_store::GatewayStore::open(&gateway_dir)?;

    let secret_value = if let Some(env_var) = from_env {
        std::env::var(env_var).map_err(|_| {
            anyhow::anyhow!("Environment variable '{}' is not set", env_var)
        })?
    } else if let Some(val) = value {
        val.clone()
    } else {
        print!("Enter secret value for '{}': ", secret_name);
        std::io::stdout().flush().ok();
        let input = rpassword::prompt_password("").map_err(|e| {
            anyhow::anyhow!("Failed to read secret from terminal: {}", e)
        })?;
        if input.is_empty() {
            anyhow::bail!("Secret value must not be empty");
        }
        input
    };

    autonoetic_gateway::vault::ensure_default_key(&config.agents_dir)?;
    let vault_path = autonoetic_gateway::vault::default_vault_path(&config.agents_dir);
    let mut vault = autonoetic_gateway::vault::Vault::load_from_file(&vault_path)?;
    vault.set_secret(secret_name, secret_value);
    vault.persist_to_file(&vault_path)?;

    let cred_id = credential_id
        .clone()
        .unwrap_or_else(|| format!("cred_{}", uuid::Uuid::new_v4().to_string().replace('-', "")));

    let cred = autonoetic_types::agent::CredentialRecord {
        credential_id: cred_id.clone(),
        service: service.to_string(),
        secret_name: secret_name.to_string(),
        inject_as: inject_as.clone(),
        created_by_agent: None,
        expires_at: expires_at.clone(),
        shared_with: vec![],
        allowed_hosts: allowed_hosts.clone().unwrap_or_default(),
        refresh_token_secret_name: None,
        refresh_url: None,
        refresh_method: None,
        refresh_headers: None,
        refresh_extract_access_token: None,
        refresh_extract_refresh_token: None,
        refresh_extract_expires_in: None,
        label: None,
    };
    store.upsert_credential(&cred)?;

    println!("Stored credential '{}' for service '{}' (secret_name: {})", cred_id, service, secret_name);
    Ok(())
}

fn handle_credential_list(
    config_path: &Path,
    service_filter: &Option<String>,
    json: bool,
) -> anyhow::Result<()> {
    let config = autonoetic_gateway::config::load_config(config_path)?;
    let gateway_dir = autonoetic_gateway::execution::gateway_root_dir(&config);
    let store = autonoetic_gateway::scheduler::gateway_store::GatewayStore::open(&gateway_dir)?;

    let credentials: Vec<autonoetic_types::agent::CredentialRecord> = if let Some(svc) =
        service_filter
    {
        store.list_credentials_by_service(svc)?
    } else {
        store.list_all_credentials()?
    };

    if json {
        println!("{}", serde_json::to_string_pretty(&credentials)?);
    } else if credentials.is_empty() {
        println!("No credentials found.");
    } else {
        println!(
            "{:<36} {:<24} {:<24} {:<16} EXPIRES",
            "CREDENTIAL ID", "SERVICE", "SECRET NAME", "INJECT AS"
        );
        for cred in &credentials {
            let inject = cred.inject_as.as_deref().unwrap_or("-");
            let expires = cred.expires_at.as_deref().unwrap_or("-");
            println!(
                "{:<36} {:<24} {:<24} {:<16} {}",
                cred.credential_id, cred.service, cred.secret_name, inject, expires
            );
        }
    }
    Ok(())
}

fn handle_credential_rm(config_path: &Path, credential_id: &str) -> anyhow::Result<()> {
    let config = autonoetic_gateway::config::load_config(config_path)?;
    let gateway_dir = autonoetic_gateway::execution::gateway_root_dir(&config);
    let store = autonoetic_gateway::scheduler::gateway_store::GatewayStore::open(&gateway_dir)?;

    let deleted = store.delete_credential(credential_id)?;
    if deleted {
        println!("Removed credential '{}'", credential_id);
    } else {
        println!("Credential '{}' not found", credential_id);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use autonoetic_gateway::artifact_store::ArtifactStore;
    use autonoetic_gateway::llm::{
        CompletionRequest, CompletionResponse, LlmDriver, StopReason, TokenUsage, ToolCall,
    };
    use autonoetic_gateway::runtime::content_store::ContentStore;
    use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
    use autonoetic_types::agent_revision::{
        AgentAliasRecord, AgentRevisionRecord, AgentRevisionStatus, PromotionKind, PromotionRecord,
    };
    use autonoetic_types::artifact::ArtifactKind;
    use secrecy::ExposeSecret;
    use serial_test::serial;
    use tempfile::tempdir;

    struct DenySandboxExecDriver;

    #[async_trait::async_trait]
    impl LlmDriver for DenySandboxExecDriver {
        async fn complete(
            &self,
            request: &CompletionRequest,
        ) -> anyhow::Result<CompletionResponse> {
            if !request.tools.iter().any(|t| t.name == "sandbox_exec") {
                anyhow::bail!("sandbox.exec not exposed to model");
            }
            Ok(CompletionResponse {
                text: String::new(),
                tool_calls: vec![ToolCall {
                    id: "call_1".to_string(),
                    name: "sandbox_exec".to_string(),
                    arguments: serde_json::json!({
                        "command": "echo blocked"
                    })
                    .to_string(),
                }],
                reasoning_content: None,
                reasoning_details: None,
                stop_reason: StopReason::ToolUse,
                usage: TokenUsage::default(),
            })
        }
    }

    #[test]
    fn cli_session_close_outcome_headless_mapping_is_closed_and_stable() {
        use autonoetic_gateway::runtime::lifecycle::TurnOutcome;

        let completed = session_close_outcome_from_headless_turn_outcome(
            &TurnOutcome::Completed(Some("ok".to_string())),
        );
        let completed_empty =
            session_close_outcome_from_headless_turn_outcome(&TurnOutcome::Completed(None));
        let suspended = session_close_outcome_from_headless_turn_outcome(&TurnOutcome::Suspended {
            approval_request_id: "apr-1".to_string(),
        });
        let suspended_user = session_close_outcome_from_headless_turn_outcome(
            &TurnOutcome::SuspendedUserInput {
                interaction_id: "ui-1".to_string(),
            },
        );
        let escalated = session_close_outcome_from_headless_turn_outcome(&TurnOutcome::Escalated {
            escalation_request_id: "esc-1".to_string(),
        });

        assert_eq!(completed.as_str(), "headless_complete");
        assert_eq!(completed_empty.as_str(), "headless_complete_empty");
        assert_eq!(suspended.as_str(), "headless_suspended");
        assert_eq!(suspended_user.as_str(), "headless_suspended_user_input");
        assert_eq!(escalated.as_str(), "headless_escalated");
    }

    #[test]
    fn cli_session_close_outcome_interactive_tags_are_stable() {
        assert_eq!(
            SessionCloseOutcome::InteractiveError.as_str(),
            "interactive_error"
        );
        assert_eq!(
            SessionCloseOutcome::InteractiveExit.as_str(),
            "interactive_exit"
        );
    }

    #[tokio::test]
    async fn test_agent_run_path_enforces_sandbox_shell_policy() {
        let temp = tempdir().expect("tempdir should create");
        let agents_dir = temp.path().join("agents");
        let agent_dir = agents_dir.join("agent_demo");
        std::fs::create_dir_all(&agent_dir).expect("agent dir should create");

        let skill = r#"---
version: "1.0"
runtime:
  engine: "autonoetic"
  gateway_version: "0.1.0"
  sdk_version: "0.1.0"
  type: "stateful"
  sandbox: "bubblewrap"
  runtime_lock: "runtime.lock"
agent:
  id: "agent_demo"
  name: "Agent Demo"
  description: "Demo agent"
capabilities:
  - type: "CodeExecution"
    patterns:
      - "python3 scripts/*"
---
# Agent Demo
Use tools when needed.
"#;
        std::fs::write(agent_dir.join("SKILL.md"), skill).expect("skill should write");

        let config_path = temp.path().join("config.yaml");
        let config_yaml = format!(
            "agents_dir: \"{}\"\nport: 4000\nofp_port: 4200\ntls: false\n",
            agents_dir.display()
        );
        std::fs::write(&config_path, config_yaml).expect("config should write");

        let (manifest, instructions, loaded_agent_dir) =
            load_agent_runtime_context(&config_path, "agent_demo").expect("context should load");
        let err = run_agent_with_runtime_with_driver(
            manifest,
            instructions,
            loaded_agent_dir,
            Some("start"),
            false,
            true,
            Arc::new(DenySandboxExecDriver),
            None,
            None,
        )
        .await
        .expect_err("policy denial should fail runtime");

        let es = err.to_string();
        assert!(
            es.contains("blocked by security policy (static analysis)")
                || es.contains("rule P-1.9")
                || es.contains("sandbox command denied by security policy")
                || es.contains("sandbox command denied by CodeExecution policy")
                || es.contains("LoopGuard tripped"),
            "error should reflect sandbox denial or loop break after repeated denials: {}",
            es
        );
    }

    #[test]
    fn test_init_agent_scaffold_creates_skill_and_runtime_lock() {
        let temp = tempdir().expect("tempdir should create");
        let config_path = temp.path().join("config.yaml");
        let agents_dir = temp.path().join("agents");
        let config_yaml = format!(
            "agents_dir: \"{}\"\nport: 4000\nofp_port: 4200\ntls: false\n",
            agents_dir.display()
        );
        std::fs::write(&config_path, config_yaml).expect("config should write");

        init_agent_scaffold(
            &config_path,
            "agent_bootstrap",
            Some("coder"),
            None,
            None,
            None,
        )
        .expect("scaffold should succeed");

        let agent_dir = agents_dir.join("agent_bootstrap");
        let skill =
            std::fs::read_to_string(agent_dir.join("SKILL.md")).expect("SKILL.md should exist");
        let lock = std::fs::read_to_string(agent_dir.join("runtime.lock"))
            .expect("runtime.lock should exist");

        assert!(skill.contains("id: \"agent_bootstrap\""));
        assert!(skill.contains("description: \"Software engineering autonomous agent.\""));
        assert!(lock.contains("dependencies: []"));
    }

    #[test]
    fn test_render_skill_template_supports_planner_template() {
        let skill = render_skill_template("planner.default", Some("planner"), "smart", None);
        assert!(skill.contains("agent:\n      id: \"planner.default\""));
        assert!(skill.contains("llm_preset: smart"));
        assert!(skill.contains("Front-door lead agent for ambiguous goals."));
        assert!(skill.contains("You are a planner agent."));
    }

    #[test]
    fn test_resolve_llm_config_uses_hardcoded_defaults_for_templates() {
        let config = GatewayConfig::default();

        let llm = resolve_llm_config(&config, Some("coder"), None, None, None);
        assert_eq!(llm.provider, "anthropic");
        assert_eq!(llm.model, "claude-sonnet-4-20250514");
        assert_eq!(llm.temperature, 0.1);

        let llm = resolve_llm_config(&config, Some("planner"), None, None, None);
        assert_eq!(llm.provider, "anthropic");
        assert_eq!(llm.temperature, 0.2);
    }

    #[test]
    fn test_resolve_llm_config_uses_presets_from_config() {
        let mut config = GatewayConfig::default();
        config.llm_presets.insert(
            "fast".to_string(),
            autonoetic_types::config::LlmPreset {
                provider: Some("openai".to_string()),
                model: Some("gpt-4o-mini".to_string()),
                temperature: Some(0.0),
                fallback_provider: None,
                fallback_model: None,
                chat_only: None,
                context_window_tokens: None,
                base_url: None,
                tier: None,
                cost: None,
                latency: None,
                api_key_env: None,
                thinking: None,
                routing: None,
            },
        );
        config
            .llm_preset_mapping
            .insert("coder".to_string(), "fast".to_string());

        let llm = resolve_llm_config(&config, Some("coder"), None, None, None);
        assert_eq!(llm.provider, "openai");
        assert_eq!(llm.model, "gpt-4o-mini");
        assert_eq!(llm.temperature, 0.0);
    }

    #[test]
    fn test_resolve_llm_config_planner_collaborative_uses_mapping() {
        let mut config = GatewayConfig::default();
        config.llm_presets.insert(
            "local".to_string(),
            autonoetic_types::config::LlmPreset {
                provider: Some("llamacpp".to_string()),
                model: Some("Qwen3.6-27B.gguf".to_string()),
                temperature: Some(0.2),
                fallback_provider: None,
                fallback_model: None,
                chat_only: None,
                context_window_tokens: None,
                base_url: Some("http://localhost:9878/v1/chat/completions".to_string()),
                tier: None,
                cost: None,
                latency: None,
                api_key_env: None,
                thinking: None,
                routing: None,
            },
        );
        config
            .llm_preset_mapping
            .insert("planner.collaborative".to_string(), "local".to_string());

        let llm = resolve_llm_config(&config, Some("planner.collaborative"), None, None, None);
        assert_eq!(llm.provider, "llamacpp");
        assert_eq!(llm.model, "Qwen3.6-27B.gguf");

        config.llm_preset_mapping.remove("planner.collaborative");
        config
            .llm_preset_mapping
            .insert("planner".to_string(), "local".to_string());
        let llm = resolve_llm_config(&config, Some("planner.collaborative"), None, None, None);
        assert_eq!(llm.provider, "llamacpp");
    }

    #[test]
    fn test_resolve_llm_config_cli_override_wins() {
        let config = GatewayConfig::default();

        let llm = resolve_llm_config(
            &config,
            Some("coder"),
            None,
            Some("google"),
            Some("gemini-pro"),
        );
        assert_eq!(llm.provider, "google");
        assert_eq!(llm.model, "gemini-pro");
    }

    fn write_top_level_reference_bundle(root: &std::path::Path, agent_id: &str, marker: &str) {
        let dir = root.join(agent_id);
        std::fs::create_dir_all(&dir).expect("bundle dir should create");
        std::fs::write(
            dir.join("SKILL.md"),
            format!(
                "---\nname: \"{agent_id}\"\ndescription: \"{marker}\"\nmetadata:\n  autonoetic:\n    version: \"1.0\"\n    runtime:\n      engine: \"autonoetic\"\n      gateway_version: \"0.1.0\"\n      sdk_version: \"0.1.0\"\n      type: \"stateful\"\n      sandbox: \"bubblewrap\"\n      runtime_lock: \"runtime.lock\"\n    agent:\n      id: \"{agent_id}\"\n      name: \"{agent_id}\"\n      description: \"{marker}\"\n---\n#{agent_id}\n"
            ),
        )
        .expect("skill should write");
        std::fs::write(dir.join("runtime.lock"), default_runtime_lock_contents())
            .expect("runtime.lock should write");
    }

    fn write_reference_bundle(root: &std::path::Path, group: &str, agent_id: &str, marker: &str) {
        let dir = root.join(group).join(agent_id);
        std::fs::create_dir_all(&dir).expect("bundle dir should create");
        std::fs::write(
            dir.join("SKILL.md"),
            format!(
                "---\nname: \"{agent_id}\"\ndescription: \"{marker}\"\nmetadata:\n  autonoetic:\n    version: \"1.0\"\n    runtime:\n      engine: \"autonoetic\"\n      gateway_version: \"0.1.0\"\n      sdk_version: \"0.1.0\"\n      type: \"stateful\"\n      sandbox: \"bubblewrap\"\n      runtime_lock: \"runtime.lock\"\n    agent:\n      id: \"{agent_id}\"\n      name: \"{agent_id}\"\n      description: \"{marker}\"\n---\n#{agent_id}\n"
            ),
        )
        .expect("skill should write");
        std::fs::write(dir.join("runtime.lock"), default_runtime_lock_contents())
            .expect("runtime.lock should write");
    }

    #[test]
    fn test_discover_reference_bundles_includes_top_level_digest() {
        let temp = tempdir().expect("tempdir should create");
        let reference_root = temp.path().join("reference_agents");
        write_reference_bundle(&reference_root, "lead", "planner.default", "planner");
        write_top_level_reference_bundle(&reference_root, "digest", "post-session digest");

        let bundles =
            discover_reference_bundles(&reference_root).expect("discover should succeed");
        let names: Vec<String> = bundles
            .iter()
            .filter_map(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .map(str::to_string)
            })
            .collect();
        assert!(
            names.iter().any(|name| name == "digest"),
            "expected top-level digest bundle, got: {names:?}"
        );
        assert!(
            names.iter().any(|name| name == "planner.default"),
            "expected grouped planner bundle, got: {names:?}"
        );
    }

    #[test]
    #[serial]
    fn test_handle_agent_bootstrap_installs_reference_bundles() {
        autonoetic_gateway::constitution_digest::reset_constitution_runtime_for_tests();
        let temp = tempdir().expect("tempdir should create");
        let reference_root = temp.path().join("reference_agents");
        write_reference_bundle(&reference_root, "lead", "planner.default", "planner");
        write_reference_bundle(&reference_root, "specialists", "coder.default", "coder");
        write_top_level_reference_bundle(&reference_root, "digest", "post-session digest");

        let config_path = temp.path().join("config.yaml");
        let agents_dir = temp.path().join("runtime_agents");
        std::fs::write(
            &config_path,
            format!(
                "agents_dir: \"{}\"\nport: 4000\nofp_port: 4200\ntls: false\n",
                agents_dir.display()
            ),
        )
        .expect("config should write");

        handle_agent_bootstrap(
            &config_path,
            Some(reference_root.to_str().expect("utf-8 path")),
            false,
        )
        .expect("bootstrap should succeed");

        assert!(agents_dir.join("planner.default").join("SKILL.md").exists());
        assert!(agents_dir
            .join("coder.default")
            .join("runtime.lock")
            .exists());
        assert!(
            agents_dir.join("digest").join("SKILL.md").exists(),
            "top-level digest bundle should be installed by bootstrap"
        );

        let constitution_root = agents_dir.join(".gateway").join("constitution");
        assert!(
            constitution_root.join("CURRENT").exists(),
            "bootstrap should materialize .gateway/constitution/CURRENT"
        );
        assert!(
            constitution_root.join("ACTIVE.json").exists(),
            "bootstrap should materialize .gateway/constitution/ACTIVE.json"
        );
        let active: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(constitution_root.join("ACTIVE.json"))
                .expect("ACTIVE.json should be readable"),
        )
        .expect("ACTIVE.json should decode");
        let current = std::fs::read_to_string(constitution_root.join("CURRENT"))
            .expect("CURRENT should be readable")
            .trim()
            .to_string();
        let source_path = active["source_path"]
            .as_str()
            .expect("source_path should be present");
        let lock_path = active["lock_path"]
            .as_str()
            .expect("lock_path should be present");
        assert!(
            agents_dir.join(source_path).exists(),
            "bootstrapped constitution source should exist under .gateway"
        );
        assert!(
            agents_dir.join(lock_path).exists(),
            "bootstrapped constitution lock should exist under .gateway"
        );
        let lock_value: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(agents_dir.join(lock_path))
                .expect("bootstrapped lock should be readable"),
        )
        .expect("bootstrapped lock should decode");
        assert_eq!(
            lock_value["constitution_source"].as_str().unwrap_or_default(),
            source_path,
            "bootstrapped lock must point to the bootstrapped source path"
        );
        let lock_signer_id = lock_value["signature"]["signer_id"]
            .as_str()
            .expect("bootstrapped lock must include a signer_id");
        let lock_signature_b64 = lock_value["signature"]["signature_b64"]
            .as_str()
            .expect("bootstrapped lock must include a signature");
        assert!(
            lock_signer_id.starts_with("gateway:"),
            "bootstrapped lock should be signed by local gateway identity"
        );
        let lock_struct: autonoetic_gateway::constitution_digest::ConstitutionLock =
            serde_json::from_value(lock_value.clone()).expect("bootstrapped lock should decode");
        let payload = autonoetic_gateway::constitution_digest::constitution_lock_signature_payload(
            &lock_struct,
        )
        .expect("signature payload should serialize");
        let public_key_path = agents_dir
            .join(".gateway")
            .join(autonoetic_gateway::runtime::crypto::GatewayIdentityKey::PUBLIC_FILENAME);
        let pub_bytes = std::fs::read(&public_key_path).expect("gateway public key should exist");
        assert_eq!(pub_bytes.len(), 32, "gateway public key must be 32 bytes");
        let mut public_key = [0u8; 32];
        public_key.copy_from_slice(&pub_bytes);
        assert!(
            autonoetic_gateway::runtime::crypto::verify_attestation_signature(
                &public_key,
                &payload,
                lock_signature_b64,
            )
            .expect("signature verification should run"),
            "bootstrapped lock signature should verify with local gateway public key"
        );
        assert_eq!(
            active["lock_signer_id"].as_str().unwrap_or_default(),
            lock_signer_id,
            "ACTIVE lock_signer_id must match lock signature signer"
        );
        assert_eq!(
            active["constitution_version"].as_str().unwrap_or_default(),
            current,
            "ACTIVE constitution version must match CURRENT"
        );
    }

    #[test]
    #[serial]
    fn test_install_reference_agents_creates_missing_agents_dir() {
        autonoetic_gateway::constitution_digest::reset_constitution_runtime_for_tests();
        let temp = tempdir().expect("tempdir should create");
        let reference_root = temp.path().join("reference_agents");
        write_reference_bundle(&reference_root, "lead", "planner.default", "v1");

        let agents_dir = temp.path().join("runtime_agents");
        assert!(!agents_dir.exists());

        let mut config = autonoetic_gateway::config::load_config(&temp.path().join("nope.yaml"))
            .expect("default config should load");
        config.agents_dir = agents_dir.clone();

        let install = install_reference_agents(
            &config,
            Some(reference_root.to_str().expect("utf-8 path")),
            false,
        )
        .expect("install should succeed");

        assert!(agents_dir.exists());
        assert_eq!(install.copied, 1);
        assert!(agents_dir.join("planner.default").join("SKILL.md").exists());
    }

    #[test]
    #[serial]
    fn test_handle_agent_bootstrap_overwrite_behavior() {
        autonoetic_gateway::constitution_digest::reset_constitution_runtime_for_tests();
        let temp = tempdir().expect("tempdir should create");
        let reference_root = temp.path().join("reference_agents");
        write_reference_bundle(&reference_root, "lead", "planner.default", "v1");

        let config_path = temp.path().join("config.yaml");
        let agents_dir = temp.path().join("runtime_agents");
        std::fs::write(
            &config_path,
            format!(
                "agents_dir: \"{}\"\nport: 4000\nofp_port: 4200\ntls: false\n",
                agents_dir.display()
            ),
        )
        .expect("config should write");

        handle_agent_bootstrap(
            &config_path,
            Some(reference_root.to_str().expect("utf-8 path")),
            false,
        )
        .expect("first bootstrap should succeed");

        let installed_path = agents_dir.join("planner.default").join("SKILL.md");
        let first = std::fs::read_to_string(&installed_path).expect("installed skill should read");
        assert!(first.contains("description: \"v1\""));

        let gw_config = autonoetic_gateway::config::load_config(&config_path).expect("config should load");
        let gateway_dir = autonoetic_gateway::execution::gateway_root_dir(&gw_config);
        let store = GatewayStore::open(&gateway_dir).expect("store should open");
        let v1_revisions = store.list_agent_revisions("planner.default").expect("should list");
        assert_eq!(v1_revisions.len(), 1, "first bootstrap should create exactly one revision");
        let v1_rev_id = v1_revisions[0].revision_id.clone();
        let v1_alias = store.get_agent_alias("planner.default").expect("alias should resolve");
        assert_eq!(v1_alias.unwrap().revision_id, v1_rev_id, "alias should point to v1 revision");

        // Without overwrite, files on disk stay the same and no new revision is created
        write_reference_bundle(&reference_root, "lead", "planner.default", "v2");
        handle_agent_bootstrap(
            &config_path,
            Some(reference_root.to_str().expect("utf-8 path")),
            false,
        )
        .expect("second bootstrap should succeed");
        let second = std::fs::read_to_string(&installed_path).expect("installed skill should read");
        assert!(second.contains("description: \"v1\""));
        let v1_revisions_after = store.list_agent_revisions("planner.default").expect("should list");
        assert_eq!(v1_revisions_after.len(), 1, "no overwrite should not create new revision");

        // With overwrite, disk is updated AND a new revision is created and promoted
        handle_agent_bootstrap(
            &config_path,
            Some(reference_root.to_str().expect("utf-8 path")),
            true,
        )
        .expect("overwrite bootstrap should succeed");
        let third = std::fs::read_to_string(&installed_path).expect("installed skill should read");
        assert!(third.contains("description: \"v2\""));
        let v2_revisions = store.list_agent_revisions("planner.default").expect("should list");
        assert_eq!(v2_revisions.len(), 2, "overwrite should create a second revision");
        let v2_alias = store.get_agent_alias("planner.default").expect("alias should resolve");
        assert_ne!(v2_alias.unwrap().revision_id, v1_rev_id, "alias should point to the new v2 revision");
    }

    #[test]
    #[serial]
    fn test_handle_agent_bootstrap_requires_existing_config_file() {
        autonoetic_gateway::constitution_digest::reset_constitution_runtime_for_tests();
        let temp = tempdir().expect("tempdir should create");
        let config_path = temp.path().join("missing-config.yaml");
        let reference_root = temp.path().join("reference_agents");
        write_reference_bundle(&reference_root, "lead", "planner.default", "planner");

        let err = handle_agent_bootstrap(
            &config_path,
            Some(reference_root.to_str().expect("utf-8 path")),
            false,
        )
        .expect_err("missing config should fail fast");
        assert!(err.to_string().contains("Config file not found"));
    }

    #[test]
    fn test_alias_and_promotion_history_handlers_with_gateway_store() {
        let temp = tempdir().expect("tempdir should create");
        let config_path = temp.path().join("config.yaml");
        let agents_dir = temp.path().join("runtime_agents");
        std::fs::write(
            &config_path,
            format!(
                "agents_dir: \"{}\"\nport: 4000\nofp_port: 4200\ntls: false\n",
                agents_dir.display()
            ),
        )
        .expect("config should write");

        let gateway_dir = autonoetic_gateway::execution::gateway_root_dir(
            &autonoetic_gateway::config::load_config(&config_path).expect("config should load"),
        );
        let store = GatewayStore::open(&gateway_dir).expect("gateway store should open");
        let revision = AgentRevisionRecord {
            revision_id: "rev_sha256:abc123".to_string(),
            agent_id: "planner.default".to_string(),
            base_revision_id: None,
            artifact_id: Some("art_1234".to_string()),
            content_digest: "sha256:abc123".to_string(),
            runtime_lock_hash: "sha256:lock1".to_string(),
            manifest_hash: "sha256:man1".to_string(),
            created_at: "2026-01-01T00:00:00Z".to_string(),
            created_by_type: "human".to_string(),
            created_by_id: "operator".to_string(),
            source_kind: "artifact".to_string(),
            source_ref: Some("art_1234".to_string()),
            origin_node_id: "gateway".to_string(),
            trust_domain: "local".to_string(),
            status: AgentRevisionStatus::Ready,
            metadata_json: serde_json::json!({}),
            short_id: "abc12345".to_string(),
        detected_network_hosts: None,
            signature: None,
            signer_id: None,
        };
        store
            .insert_agent_revision(&revision)
            .expect("revision should insert");
        store
            .upsert_agent_alias(&AgentAliasRecord {
                alias_id: "planner.default".to_string(),
                agent_id: "planner.default".to_string(),
                revision_id: revision.revision_id.clone(),
                updated_at: "2026-01-01T00:00:01Z".to_string(),
                updated_by_type: autonoetic_types::principal::PrincipalKind::Human.tag().to_string(),
                updated_by_id: "operator".to_string(),
                reason: Some("initial seed".to_string()),
                suspended_at: None,
                suspended_reason: None,
                suspended_by: None,
            })
            .expect("alias should upsert");
        store
            .insert_promotion_record(&PromotionRecord {
                promotion_id: "prom_1".to_string(),
                kind: PromotionKind::Promote,
                alias_id: "planner.default".to_string(),
                agent_id: "planner.default".to_string(),
                previous_revision_id: None,
                new_revision_id: revision.revision_id.clone(),
                source_eval_run_id: Some("eval_1".to_string()),
                reason: Some("seed".to_string()),
                created_at: "2026-01-01T00:00:02Z".to_string(),
                created_by_type: "human".to_string(),
                created_by_id: "operator".to_string(),
                origin_node_id: "gateway".to_string(),
                pre_authorization: None,
            })
            .expect("promotion history should insert");

        handle_agent_alias(
            &config_path,
            &AgentAliasCommands::List {
                agent_id: Some("planner.default".to_string()),
                json: true,
            },
        )
        .expect("alias list should succeed");
        handle_agent_alias(
            &config_path,
            &AgentAliasCommands::Inspect {
                alias_id: "planner.default".to_string(),
                json: true,
            },
        )
        .expect("alias inspect should succeed");
        handle_agent_promotion_history(&config_path, Some("planner.default"), true)
            .expect("promotion history should succeed");
    }

    #[test]
    fn test_agent_list_reads_aliases_from_registry_state() {
        let temp = tempdir().expect("tempdir should create");
        let config_path = temp.path().join("config.yaml");
        let agents_dir = temp.path().join("runtime_agents");
        std::fs::write(
            &config_path,
            format!(
                "agents_dir: \"{}\"\nport: 4000\nofp_port: 4200\ntls: false\n",
                agents_dir.display()
            ),
        )
        .expect("config should write");
        let config = autonoetic_gateway::config::load_config(&config_path).expect("config loads");
        let gateway_dir = autonoetic_gateway::execution::gateway_root_dir(&config);
        let store = GatewayStore::open(&gateway_dir).expect("gateway store opens");

        store
            .insert_agent_revision(&AgentRevisionRecord {
                revision_id: "rev_sha256:list123".to_string(),
                agent_id: "list.agent".to_string(),
                base_revision_id: None,
                artifact_id: Some("art_list".to_string()),
                content_digest: "sha256:list123".to_string(),
                runtime_lock_hash: "sha256:lock_list".to_string(),
                manifest_hash: "sha256:man_list".to_string(),
                created_at: "2026-01-01T00:00:00Z".to_string(),
                created_by_type: "human".to_string(),
                created_by_id: "operator".to_string(),
                source_kind: "artifact".to_string(),
                source_ref: Some("art_list".to_string()),
                origin_node_id: "gateway".to_string(),
                trust_domain: "local".to_string(),
                status: AgentRevisionStatus::Ready,
                metadata_json: serde_json::json!({}),
                short_id: "list1234".to_string(),
        detected_network_hosts: None,
                signature: None,
                signer_id: None,
            })
            .expect("revision insert should succeed");
        store
            .upsert_agent_alias(&AgentAliasRecord {
                alias_id: "list.agent".to_string(),
                agent_id: "list.agent".to_string(),
                revision_id: "rev_sha256:list123".to_string(),
                updated_at: "2026-01-01T00:00:01Z".to_string(),
                updated_by_type: autonoetic_types::principal::PrincipalKind::Human.tag().to_string(),
                updated_by_id: "operator".to_string(),
                reason: Some("seed".to_string()),
                suspended_at: None,
                suspended_reason: None,
                suspended_by: None,
            })
            .expect("alias insert should succeed");

        let rows = list_alias_rows_from_registry(&config).expect("rows should load from registry");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].alias_id, "list.agent");
        assert_eq!(rows[0].agent_id, "list.agent");
        assert_eq!(rows[0].active_revision, "rev_list1234");
        assert_eq!(rows[0].status, "Ready");
    }

    #[test]
    fn test_seed_handler_promotes_alias_to_target_revision() {
        let temp = tempdir().expect("tempdir should create");
        let config_path = temp.path().join("config.yaml");
        let agents_dir = temp.path().join("runtime_agents");
        std::fs::write(
            &config_path,
            format!(
                "agents_dir: \"{}\"\nport: 4000\nofp_port: 4200\ntls: false\n",
                agents_dir.display()
            ),
        )
        .expect("config should write");

        let gateway_dir = autonoetic_gateway::execution::gateway_root_dir(
            &autonoetic_gateway::config::load_config(&config_path).expect("config should load"),
        );
        let store = GatewayStore::open(&gateway_dir).expect("gateway store should open");
        let revision = AgentRevisionRecord {
            revision_id: "rev_sha256:seed123".to_string(),
            agent_id: "seed.agent".to_string(),
            base_revision_id: None,
            artifact_id: Some("art_seed".to_string()),
            content_digest: "sha256:seed123".to_string(),
            runtime_lock_hash: "sha256:lock_seed".to_string(),
            manifest_hash: "sha256:man_seed".to_string(),
            created_at: "2026-01-01T00:00:00Z".to_string(),
            created_by_type: "human".to_string(),
            created_by_id: "operator".to_string(),
            source_kind: "artifact".to_string(),
            source_ref: Some("art_seed".to_string()),
            origin_node_id: "gateway".to_string(),
            trust_domain: "local".to_string(),
            status: AgentRevisionStatus::Candidate,
            metadata_json: serde_json::json!({}),
            short_id: "seed1234".to_string(),
        detected_network_hosts: None,
            signature: None,
            signer_id: None,
        };
        store
            .insert_agent_revision(&revision)
            .expect("revision should insert");

        handle_agent_seed(
            &config_path,
            "seed.agent",
            &revision.revision_id,
            Some("prom_seed_test"),
            Some("deterministic test seed"),
            true,
        )
        .expect("seed command should succeed");

        let alias = store
            .resolve_alias("seed.agent")
            .expect("alias lookup should work")
            .expect("alias should exist");
        assert_eq!(alias.revision_id, revision.revision_id);

        let history = store
            .list_promotion_history("seed.agent")
            .expect("history should list");
        assert!(
            history
                .iter()
                .any(|row| row.promotion_id == "prom_seed_test"),
            "expected deterministic promotion id in history"
        );
    }

    #[test]
    fn test_revision_handlers_create_and_promote() {
        let temp = tempdir().expect("tempdir should create");
        let config_path = temp.path().join("config.yaml");
        let agents_dir = temp.path().join("runtime_agents");
        std::fs::write(
            &config_path,
            format!(
                "agents_dir: \"{}\"\nport: 4000\nofp_port: 4200\ntls: false\n",
                agents_dir.display()
            ),
        )
        .expect("config should write");

        let config = autonoetic_gateway::config::load_config(&config_path).expect("config loads");
        let gateway_dir = autonoetic_gateway::execution::gateway_root_dir(&config);
        std::fs::create_dir_all(&gateway_dir).expect("gateway dir should exist");
        let content_store = ContentStore::new(&gateway_dir).expect("content store");
        let artifact_store = ArtifactStore::new(&gateway_dir).expect("artifact store");
        let session_id = "rev-handler-test";

        let skill_md = r#"---
version: "1.0"
runtime:
  engine: "autonoetic"
  gateway_version: "0.1.0"
  sdk_version: "0.1.0"
  type: "stateful"
  sandbox: "bubblewrap"
  runtime_lock: "runtime.lock"
agent:
  id: "revision.handler"
  name: "Revision Handler"
  description: "Revision handler test"
---
# Revision Handler
"#;
        let runtime_lock = r#"gateway:
  artifact: "gateway"
  version: "0.1.0"
  sha256: "sha256:gateway"
sdk:
  version: "0.1.0"
sandbox:
  backend: "bubblewrap"
dependencies: []
artifacts: []
layers: []
"#;
        for (name, content) in [
            ("SKILL.md", skill_md.as_bytes()),
            ("runtime.lock", runtime_lock.as_bytes()),
            ("main.py", b"print('hello')".as_ref()),
        ] {
            let handle = content_store.write(content).expect("write content");
            content_store
                .register_name(session_id, name, &handle)
                .expect("register name");
        }
        let bundle = artifact_store
            .build_with_kind(
                &[
                    "SKILL.md".to_string(),
                    "runtime.lock".to_string(),
                    "main.py".to_string(),
                ],
                Some(&["main.py".to_string()]),
                None,
                ArtifactKind::AgentBundle,
                session_id,
            )
            .expect("build bundle");

        handle_agent_revision(
            &config_path,
            &AgentRevisionCommands::Create {
                agent_id: "revision.handler".to_string(),
                artifact_id: bundle.artifact_id.clone(),
                base_revision_id: None,
                summary: Some("create revision".to_string()),
                json: true,
            },
        )
        .expect("revision create should succeed");

        let store = GatewayStore::open(&gateway_dir).expect("gateway store opens");
        let revisions = store
            .list_agent_revisions("revision.handler")
            .expect("list revisions");
        assert_eq!(revisions.len(), 1);

        handle_agent_revision(
            &config_path,
            &AgentRevisionCommands::Promote {
                agent_id: "revision.handler".to_string(),
                revision_id: revisions[0].revision_id.clone(),
                reason: Some("activate".to_string()),
                required_eval_run_id: None,
                json: true,
            },
        )
        .expect("revision promote should succeed");

        let alias = store
            .resolve_alias("revision.handler")
            .expect("alias lookup")
            .expect("alias should exist");
        assert_eq!(alias.revision_id, revisions[0].revision_id);
    }

    #[test]
    fn test_import_skill_writes_skill_md_with_agentskills_metadata() {
        use crate::cli::common::TrustMode;
        use tempfile::tempdir;

        let temp = tempdir().unwrap();
        let skill_dir = temp.path().join("external-skill");
        std::fs::create_dir_all(&skill_dir).unwrap();

        let skill_content = r#"---
name: "external-git-helper"
description: "A git helper from agentskills.io"
license: "MIT"
compatibility: "claude-code"
allowed-tools:
  - "Bash(git:*)"
  - "Read"
  - "Write"
---

# External Git Helper

Use Bash(git log) to inspect history.
"#;
        std::fs::write(skill_dir.join("SKILL.md"), skill_content).unwrap();

        let config_dir = temp.path().join("config");
        std::fs::create_dir_all(&config_dir).unwrap();
        let config_path = config_dir.join("config.yaml");
        std::fs::write(
            &config_path,
            r#"
llm_presets:
  agentic:
    provider: openai
    model: gpt-4o
    temperature: 0.2
"#,
        )
        .unwrap();

        let target_agents_dir = config_dir.join("agents");
        let result = handle_agent_import_skill(
            &config_path,
            skill_dir.to_str().unwrap(),
            "imported.git-helper",
            TrustMode::Strict,
            None,
            None,
        );
        assert!(result.is_ok(), "import should succeed: {:?}", result.err());

        let target_skill_path = target_agents_dir
            .join("imported-git-helper")
            .join("SKILL.md");
        assert!(target_skill_path.exists(), "imported SKILL.md should exist");

        let written = std::fs::read_to_string(&target_skill_path).unwrap();

        assert!(
            written.contains("name: \"imported.git-helper\""),
            "agent_id should be rewritten: {}",
            written
        );
        assert!(
            written.contains("license: \"MIT\""),
            "license should be preserved: {}",
            written
        );
        assert!(
            written.contains("compatibility: \"claude-code\""),
            "compatibility should be preserved: {}",
            written
        );
        assert!(
            written.contains("allowed-tools:") && written.contains("Bash(git:*)"),
            "allowed-tools should be preserved: {}",
            written
        );
        assert!(
            written.contains("engine: \"autonoetic\""),
            "autonoetic runtime should be present: {}",
            written
        );

        let (reparsed, _body) =
            autonoetic_gateway::runtime::parser::SkillParser::parse(&written).unwrap();
        assert_eq!(reparsed.agent.id, "imported.git-helper");
        assert!(
            reparsed.agentskills_import.is_some(),
            "agentskills_import should be reconstructable from reparsed manifest"
        );
        let import = reparsed.agentskills_import.unwrap();
        assert_eq!(import.license.as_deref(), Some("MIT"));
        assert_eq!(import.compatibility.as_deref(), Some("claude-code"));
        assert!(import.allowed_tools.contains(&"Bash(git:*)".to_string()));
        assert!(import.needs_tool_bridging);
    }

    #[test]
    fn test_import_skill_copies_resource_directories() {
        use crate::cli::common::TrustMode;
        use tempfile::tempdir;

        let temp = tempdir().unwrap();
        let skill_dir = temp.path().join("external-skill");
        std::fs::create_dir_all(skill_dir.join("scripts")).unwrap();
        std::fs::create_dir_all(skill_dir.join("references")).unwrap();
        std::fs::write(
            skill_dir.join("scripts/helper.sh"),
            "#!/bin/bash\necho hello\n",
        )
        .unwrap();
        std::fs::write(skill_dir.join("references/doc.txt"), "Reference docs\n").unwrap();

        let skill_content = r#"---
name: "resource-skill"
description: "Skill with resources"
---

# Resource Skill
"#;
        std::fs::write(skill_dir.join("SKILL.md"), skill_content).unwrap();

        let config_dir = temp.path().join("config");
        std::fs::create_dir_all(&config_dir).unwrap();
        let config_path = config_dir.join("config.yaml");
        std::fs::write(
            &config_path,
            r#"
llm_presets:
  agentic:
    provider: openai
    model: gpt-4o
    temperature: 0.2
"#,
        )
        .unwrap();

        let target_agents_dir = config_dir.join("agents");
        let result = handle_agent_import_skill(
            &config_path,
            skill_dir.to_str().unwrap(),
            "imported.resource-skill",
            TrustMode::Generous,
            None,
            None,
        );
        assert!(result.is_ok(), "import should succeed: {:?}", result.err());

        let target_dir = target_agents_dir.join("imported-resource-skill");
        assert!(
            target_dir.join("scripts/helper.sh").exists(),
            "scripts/ should be copied"
        );
        assert!(
            target_dir.join("references/doc.txt").exists(),
            "references/ should be copied"
        );

        let script_content = std::fs::read_to_string(target_dir.join("scripts/helper.sh")).unwrap();
        assert!(
            script_content.contains("echo hello"),
            "file content should be preserved"
        );
    }

    #[test]
    fn test_import_skill_trust_mode_strict_adds_approval_capability() {
        use crate::cli::common::TrustMode;
        use tempfile::tempdir;

        let temp = tempdir().unwrap();
        let skill_dir = temp.path().join("external-skill");
        std::fs::create_dir_all(&skill_dir).unwrap();

        let skill_content = r#"---
name: "strict-skill"
description: "Skill under strict trust"
allowed-tools:
  - "Bash(*)"
  - "WebSearch"
---

# Strict Skill
"#;
        std::fs::write(skill_dir.join("SKILL.md"), skill_content).unwrap();

        let config_dir = temp.path().join("config");
        std::fs::create_dir_all(&config_dir).unwrap();
        let config_path = config_dir.join("config.yaml");
        std::fs::write(
            &config_path,
            r#"
llm_presets:
  agentic:
    provider: openai
    model: gpt-4o
    temperature: 0.2
"#,
        )
        .unwrap();

        let target_agents_dir = config_dir.join("agents");
        let result = handle_agent_import_skill(
            &config_path,
            skill_dir.to_str().unwrap(),
            "imported.strict-skill",
            TrustMode::Strict,
            None,
            None,
        );
        assert!(result.is_ok(), "import should succeed: {:?}", result.err());

        let target_skill_path = target_agents_dir
            .join("imported-strict-skill")
            .join("SKILL.md");
        let written = std::fs::read_to_string(&target_skill_path).unwrap();

        assert!(
            written.contains("ApprovalQueue"),
            "Strict mode should add ApprovalQueue capability: {}",
            written
        );

        let (reparsed, _body) =
            autonoetic_gateway::runtime::parser::SkillParser::parse(&written).unwrap();
        assert!(
            reparsed.capabilities.iter().any(|c| matches!(
                c,
                autonoetic_types::capability::Capability::ApprovalQueue { .. }
            )),
            "reparsed manifest should have ApprovalQueue capability"
        );
    }

    #[test]
    #[serial]
    fn test_credential_put_list_rm_roundtrip() {
        std::env::remove_var("AUTONOETIC_VAULT_KEY");
        std::env::remove_var("AUTONOETIC_VAULT_KEY_PATH");
        let temp = tempdir().expect("tempdir should create");
        let config_path = temp.path().join("config.yaml");
        let agents_dir = temp.path().join("runtime_agents");
        std::fs::write(
            &config_path,
            format!(
                "agents_dir: \"{}\"\nport: 4000\nofp_port: 4200\ntls: false\n",
                agents_dir.display()
            ),
        )
        .expect("config should write");

        handle_credential_put(
            &config_path,
            "openweathermap",
            "OPENWEATHER_API_KEY",
            &None,
            &Some("test-secret-123".to_string()),
            &Some("cred_test001".to_string()),
            &None,
            &Some(vec!["api.openweathermap.org".to_string()]),
            &None,
        )
        .expect("credential put should succeed");

        let config = autonoetic_gateway::config::load_config(&config_path).unwrap();
        let gateway_dir = autonoetic_gateway::execution::gateway_root_dir(&config);
        let store = autonoetic_gateway::scheduler::gateway_store::GatewayStore::open(&gateway_dir)
            .unwrap();

        let cred = store.get_credential("cred_test001").unwrap().expect("credential should exist");
        assert_eq!(cred.service, "openweathermap");
        assert_eq!(cred.secret_name, "OPENWEATHER_API_KEY");
        assert_eq!(cred.allowed_hosts, vec!["api.openweathermap.org"]);

        let vault_path = autonoetic_gateway::vault::default_vault_path(&config.agents_dir);
        let vault = autonoetic_gateway::vault::Vault::load_from_file(&vault_path).unwrap();
        assert_eq!(
            vault.get_secret("OPENWEATHER_API_KEY").unwrap().expose_secret(),
            "test-secret-123"
        );

        let all = store.list_all_credentials().unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].credential_id, "cred_test001");

        let by_service = store.list_credentials_by_service("openweathermap").unwrap();
        assert_eq!(by_service.len(), 1);

        let empty = store.list_credentials_by_service("nonexistent").unwrap();
        assert!(empty.is_empty());

        handle_credential_rm(&config_path, "cred_test001").expect("credential rm should succeed");
        assert!(store.get_credential("cred_test001").unwrap().is_none());
    }

    #[test]
    #[serial]
    fn test_credential_put_from_env() {
        std::env::remove_var("AUTONOETIC_VAULT_KEY");
        std::env::remove_var("AUTONOETIC_VAULT_KEY_PATH");
        let temp = tempdir().expect("tempdir should create");
        let config_path = temp.path().join("config.yaml");
        let agents_dir = temp.path().join("runtime_agents");
        std::fs::write(
            &config_path,
            format!(
                "agents_dir: \"{}\"\nport: 4000\nofp_port: 4200\ntls: false\n",
                agents_dir.display()
            ),
        )
        .expect("config should write");

        std::env::set_var("TEST_AUTONOETIC_CREDO", "env-secret-value");
        handle_credential_put(
            &config_path,
            "testservice",
            "TEST_KEY",
            &Some("TEST_AUTONOETIC_CREDO".to_string()),
            &None,
            &Some("cred_envtest".to_string()),
            &None,
            &None,
            &None,
        )
        .expect("credential put --from-env should succeed");
        std::env::remove_var("TEST_AUTONOETIC_CREDO");

        let config = autonoetic_gateway::config::load_config(&config_path).unwrap();
        let vault_path = autonoetic_gateway::vault::default_vault_path(&config.agents_dir);
        let vault = autonoetic_gateway::vault::Vault::load_from_file(&vault_path).unwrap();
        assert_eq!(
            vault.get_secret("TEST_KEY").unwrap().expose_secret(),
            "env-secret-value"
        );
    }
}
