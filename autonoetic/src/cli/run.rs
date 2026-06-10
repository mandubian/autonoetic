//! One-command start: bootstrap + gateway + chat in a single invocation.
//!
//! `autonoetic run` detects available LLM API keys, generates a minimal config
//! if none exists, bootstraps agents, starts the gateway in-process, and opens
//! chat — all without requiring the user to understand the decomposed commands.

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

fn default_config_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".autonoetic")
}

/// Names in-repo agent bundles reference via `llm_preset` (Phase 2).
/// Starter `autonoetic run` config defines each as a fixed preset backed by the
/// operator's discovered model so bootstrap agents resolve without editing SKILL.md.
fn starter_llm_presets_and_mapping_yaml(
    provider: &str,
    model: &str,
    base_url_line: &str,
    context_window_line: &str,
    thinking_block: &str,
) -> String {
    fn fixed_preset(
        provider: &str,
        model: &str,
        temperature: f64,
        base_url_line: &str,
        context_window_line: &str,
        extra: &str,
    ) -> String {
        format!(
            "    provider: \"{provider}\"\n    model: \"{model}\"\n    temperature: {temperature}{base_url}{context_window}{extra}\n",
            provider = provider,
            model = model,
            temperature = temperature,
            base_url = base_url_line,
            context_window = context_window_line,
            extra = extra,
        )
    }

    let smart_extra = thinking_block;
    let agentic_extra = thinking_block;

    format!(
        r#"llm_presets:
  default:
{default_body}
  smart:
{smart_body}
  coding:
{coding_body}
  agentic:
{agentic_body}
  research:
{research_body}
  budget:
{budget_body}
  haiku:
{haiku_body}
  fallback:
{fallback_body}

llm_preset_mapping:
  planner: smart
  planner.collaborative: smart
  coder: coding
  executor: coding
  researcher: research
  debugger: coding
  evaluator: coding
  architect: coding
  auditor: coding
  specialized_builder: agentic
  packager: agentic
  registration: agentic
  agent-factory: agentic
  discovery: research
  memory-curator: agentic
  evolution-steward: agentic
  evolution-orchestrator: agentic
  agent-adapter: agentic
  context_compression: haiku
  default: fallback
"#,
        default_body = fixed_preset(
            provider,
            model,
            0.2,
            base_url_line,
            context_window_line,
            thinking_block,
        ),
        smart_body = fixed_preset(
            provider,
            model,
            0.2,
            base_url_line,
            context_window_line,
            smart_extra,
        ),
        coding_body = fixed_preset(provider, model, 0.1, base_url_line, context_window_line, ""),
        agentic_body = fixed_preset(
            provider,
            model,
            0.2,
            base_url_line,
            context_window_line,
            agentic_extra,
        ),
        research_body = fixed_preset(provider, model, 0.3, base_url_line, context_window_line, ""),
        budget_body = fixed_preset(provider, model, 0.2, base_url_line, context_window_line, ""),
        haiku_body = fixed_preset(provider, model, 0.2, base_url_line, context_window_line, ""),
        fallback_body = fixed_preset(provider, model, 0.2, base_url_line, context_window_line, ""),
    )
}

async fn ensure_config(config_path: &Path) -> anyhow::Result<()> {
    if config_path.exists() {
        return Ok(());
    }

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()?;

    let (provider, _original_entry, model, base_url) = super::model_discovery::interactive_select(&client).await?;

    let context_window_tokens = match base_url.as_deref() {
        Some(url) => {
            autonoetic_gateway::fetch_context_window_tokens(&client, url, &model).await
        }
        None => None,
    };

    let config_dir = config_path.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(config_dir)?;

    let agents_dir = config_dir.join("agents");
    let agents_dir_str = agents_dir.to_string_lossy();

    let base_url_line = match base_url {
        Some(ref url) => format!("\n    base_url: \"{}\"", url),
        None => String::new(),
    };
    let context_window_line = context_window_tokens
        .map(|tokens| format!("\n    context_window_tokens: {tokens}"))
        .unwrap_or_default();

    // Enable reasoning by default for providers whose API accepts it.
    // OpenRouter silently ignores `reasoning` on non-reasoning models, so
    // emitting it is safe even when the chosen model lacks a reasoning mode.
    // OpenAI is similar but gated by model name inside the driver.
    let thinking_block = match provider.as_str() {
        "openai" | "codex" | "openrouter" => {
            "\n    # Extended reasoning. Drop or comment out to disable.\n    # Effort levels: low | medium | high | xhigh\n    thinking:\n      effort: high"
        }
        _ => "",
    };

    let llm_section = starter_llm_presets_and_mapping_yaml(
        &provider,
        &model,
        &base_url_line,
        &context_window_line,
        thinking_block,
    );

    let config_content = format!(
        r#"# Auto-generated by `autonoetic run` — customize as needed.
# Full reference: autonoetic agent init-config
# Agent bundles declare llm_preset (smart, coding, …); each preset below uses your
# selected model. Edit individual presets in llm_presets to split models per role.
agents_dir: "{agents_dir}"
port: 4000
background_scheduler_enabled: true
profile: starter

digest_agent:
  enabled: true
  llm_preset: haiku

auto_learning:
  enabled: true

{llm_section}"#,
        agents_dir = agents_dir_str,
        llm_section = llm_section,
    );

    std::fs::write(config_path, &config_content)?;
    eprintln!("\n  Config written to {}", config_path.display());
    if let Some(ref url) = base_url {
        eprintln!("  Base URL: {}", url);
    }
    if let Some(tokens) = context_window_tokens {
        eprintln!("  Context window: {} tokens (probed from model server)", tokens);
    }

    prompt_persona(config_dir)?;

    Ok(())
}

fn prompt_persona(config_dir: &Path) -> anyhow::Result<()> {
    let persona_path = config_dir.join("persona.md");
    if persona_path.exists() {
        return Ok(());
    }

    let mut stderr = std::io::stderr();
    writeln!(
        stderr,
        "\n  Optional: tell the agents about yourself so they can adapt."
    )?;
    writeln!(
        stderr,
        "  Examples: your role, tech stack, language preferences, communication style."
    )?;
    writeln!(stderr, "  Press Enter to skip.\n")?;
    write!(stderr, "  About you: ")?;
    stderr.flush()?;

    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    let trimmed = line.trim();

    if !trimmed.is_empty() {
        std::fs::write(&persona_path, trimmed)?;
        eprintln!("  Persona saved to {}", persona_path.display());
        eprintln!("  You can update it anytime with `/persona <text>` in chat.");
    }

    Ok(())
}

fn ensure_bootstrap(config_path: &Path, overwrite: bool) -> anyhow::Result<()> {
    let config = autonoetic_gateway::config::load_config(config_path)?;
    let agents_populated = if config.agents_dir.exists() {
        let mut has_agent = false;
        if let Ok(entries) = config.agents_dir.read_dir() {
            for entry in entries.flatten() {
                if entry.file_type().map_or(false, |t| t.is_dir())
                    && entry.path().join("SKILL.md").exists()
                {
                    has_agent = true;
                    break;
                }
            }
        }
        has_agent
    } else {
        false
    };
    if agents_populated && !overwrite {
        return Ok(());
    }
    eprintln!("  Bootstrapping agents...");
    super::agent::handle_agent_bootstrap(config_path, None, overwrite)?;
    Ok(())
}

pub async fn refresh_models(config_path: &Path) -> anyhow::Result<()> {
    let config = autonoetic_gateway::config::load_config(config_path)?;
    let agents_dir = &config.agents_dir;
    anyhow::ensure!(
        agents_dir.exists(),
        "Agents directory not found at {}. Run `autonoetic run` first.",
        agents_dir.display()
    );

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new());

    let (provider, original_entry, model, base_url) = match super::model_discovery::interactive_select(&client).await {
        Ok(result) => result,
        Err(e) => {
            eprintln!("  Model selection skipped ({}). Using current config.", e);
            return Ok(());
        }
    };

    let current = std::fs::read_to_string(config_path).unwrap_or_default();

    let re_provider = regex::Regex::new(r#"(provider:\s*)"[^"]*""#).unwrap();
    let re_model = regex::Regex::new(r#"(model:\s*)"[^"]*""#).unwrap();
    let re_base_url = regex::Regex::new(r#"(base_url:\s*)"[^"]*""#).unwrap();

    let mut updated = current.clone();
    if let Some(cap) = re_provider.captures(&updated) {
        updated = updated.replacen(&cap[0], &format!("provider: \"{}\"", provider), 1);
    }
    if let Some(cap) = re_model.captures(&updated) {
        updated = updated.replacen(&cap[0], &format!("model: \"{}\"", model), 1);
    }
    match (&base_url, re_base_url.captures(&updated)) {
        (Some(url), Some(cap)) => {
            updated = updated.replacen(&cap[0], &format!("base_url: \"{}\"", url), 1);
        }
        (Some(url), None) => {
            if let Some(re) = regex::Regex::new(r#"(model:\s*"[^"]*"\s*\n)"#).ok() {
                updated = re.replace(&updated, format!("${{1}}    base_url: \"{}\"\n", url)).to_string();
            }
        }
        (None, Some(_)) => {
            if let Some(re) = regex::Regex::new(r#"(?m)^\s*base_url:\s*"[^"]*"\s*\n?"#).ok() {
                updated = re.replace(&updated, "").to_string();
            }
        }
        (None, None) => {}
    }

    if let Some(url) = base_url.as_deref().or_else(|| {
        regex::Regex::new(r#"base_url:\s*"([^"]+)""#)
            .ok()
            .and_then(|re| re.captures(&updated))
            .and_then(|cap| cap.get(1))
            .map(|m| m.as_str())
    }) {
        if let Some(tokens) =
            autonoetic_gateway::fetch_context_window_tokens(&client, url, &model).await
        {
            updated = autonoetic_gateway::patch_context_window_tokens_in_yaml(&updated, tokens);
            eprintln!("  Context window: {} tokens (probed from model server)", tokens);
        }
    }

    if updated != current {
        std::fs::write(config_path, &updated)?;
        eprintln!("  Config updated: provider={}, model={}", provider, model);
        if let Some(ref url) = base_url {
            eprintln!("  Base URL: {}", url);
        }
    } else {
        eprintln!("  Config unchanged: provider={}, model={}", provider, model);
    }

    let config = autonoetic_gateway::config::load_config(config_path)?;

    let resolved = super::agent::resolve_llm_config(&config, Some("planner"), None, None, None);
    eprintln!(
        "  Resolved preset: provider={}, model={}, base_url={:?}",
        resolved.provider, resolved.model, resolved.base_url
    );

    let gateway_dir = autonoetic_gateway::execution::gateway_root_dir(&config);
    let activated = autonoetic_gateway::bootstrap_agents(&config, &gateway_dir)?;
    eprintln!(
        "  Re-activated {} agent revision(s) with updated config presets.",
        activated
    );
    Ok(())
}

pub async fn handle_run(
    config_override: Option<&str>,
    args: &super::common::RunArgs,
) -> anyhow::Result<()> {
    let config_path = match config_override {
        Some(p) => PathBuf::from(p),
        None => default_config_dir().join("config.yaml"),
    };

    ensure_config(&config_path).await?;
    ensure_bootstrap(&config_path, args.overwrite)?;
    if args.refresh_models {
        refresh_models(&config_path).await?;
    }

    if std::env::var("AUTONOETIC_SHARED_SECRET").is_err() {
        let secret = uuid::Uuid::new_v4().to_string();
        std::env::set_var("AUTONOETIC_SHARED_SECRET", &secret);
        eprintln!("  Generated ephemeral shared secret for this session.");
    }

    let resolved_agent_id = if args.collaborative {
        Some("planner.collaborative".to_string())
    } else {
        args.agent_id.clone()
    };

    let session_id = args.session_id.clone().unwrap_or_else(|| {
        format!("session-{}", &uuid::Uuid::new_v4().to_string()[..8])
    });

    let chat_args = super::common::ChatArgs {
        agent_id: resolved_agent_id.clone(),
        sender_id: None,
        channel_id: None,
        session_id: Some(session_id.clone()),
        resume: args.resume,
        test_mode: false,
    };

    let config_path_clone = config_path.clone();
    let gateway_handle = tokio::spawn(async move {
        if let Err(e) = super::gateway::handle_gateway_start(
            &config_path_clone,
            false,
            None,
            false,
            None,
        )
        .await
        {
            eprintln!("Gateway error: {e}");
        }
    });

    let gateway_config = autonoetic_gateway::config::load_config(&config_path)?;
    let gateway_port = gateway_config.port;
    let ready = wait_for_gateway_ready(gateway_port, std::time::Duration::from_secs(30)).await;
    if !ready {
        eprintln!("Warning: gateway did not become ready within timeout. Chat may fail to connect.");
    }

    if args.collaborative {
        eprintln!("  Mode: collaborative (planner.collaborative — PlanFrame, workbench, /wb, /return)");
    }

    let log_dir = gateway_config.agents_dir.join(".gateway").join("logs");
    eprintln!(
        "  Tracing output: {0} — run `tail -f {0}/*.log` in another terminal for live gateway logs.",
        log_dir.display()
    );

    if args.room {
        super::terminal::require_interactive_terminal("Session Room")?;
        let resolved_target = resolved_agent_id.as_deref().unwrap_or("planner.default");
        let room_args = super::common::RoomArgs {
            root_session_id: Some(session_id),
            min_altitude: "normal".to_string(),
            resume: false,
            agent: Some(resolved_target.to_string()),
            follow: true,
            tui: true,
            limit: 200,
        };

        eprintln!("  Session Room: {} (agent: {})", room_args.root_session_id.as_deref().unwrap_or("?"), resolved_target);
        eprintln!("  Press 'i' to send a message. Press '/' for slash commands. Press 'q' to quit.");

        let result = super::room::handle_room_with_target(
            &config_path,
            &room_args,
            Some(resolved_target.to_string()),
        )
        .await;

        gateway_handle.abort();
        return result;
    }

    super::terminal::require_interactive_terminal("Chat")?;
    let result = super::chat::handle_chat(&config_path, &chat_args).await;

    gateway_handle.abort();
    result
}

async fn wait_for_gateway_ready(port: u16, timeout: std::time::Duration) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    let addr = format!("127.0.0.1:{port}");
    while tokio::time::Instant::now() < deadline {
        if tokio::net::TcpStream::connect(&addr).await.is_ok() {
            return true;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    false
}
