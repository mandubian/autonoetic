//! One-command start: bootstrap + gateway + chat in a single invocation.
//!
//! `autonoetic run` detects available LLM API keys, generates a minimal config
//! if none exists, bootstraps agents, starts the gateway in-process, and opens
//! chat — all without requiring the user to understand the decomposed commands.

use std::path::{Path, PathBuf};

fn default_config_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".autonoetic")
}

fn detect_llm_provider() -> Option<(&'static str, &'static str)> {
    if std::env::var("ANTHROPIC_API_KEY").is_ok() {
        Some(("anthropic", "claude-sonnet-4-20250514"))
    } else if std::env::var("OPENROUTER_API_KEY").is_ok() {
        Some(("openrouter", "anthropic/claude-sonnet-4"))
    } else if std::env::var("OPENAI_API_KEY").is_ok() {
        Some(("openai", "gpt-4o"))
    } else {
        None
    }
}

fn ensure_config(config_path: &Path) -> anyhow::Result<()> {
    if config_path.exists() {
        return Ok(());
    }

    let (provider, model) = detect_llm_provider().ok_or_else(|| {
        anyhow::anyhow!(
            "No LLM API key found in environment.\n\
             Set one of: ANTHROPIC_API_KEY, OPENROUTER_API_KEY, or OPENAI_API_KEY\n\
             then re-run `autonoetic run`."
        )
    })?;

    let config_dir = config_path.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(config_dir)?;

    let agents_dir = config_dir.join("agents");
    let agents_dir_str = agents_dir.to_string_lossy();

    let config_content = format!(
        r#"# Auto-generated minimal config — customize as needed.
# Full reference: autonoetic agent init-config
agents_dir: "{agents_dir}"
port: 4000
background_scheduler_enabled: true
profile: starter

digest_agent:
  enabled: true

auto_learning:
  enabled: true

llm_presets:
  default:
    provider: "{provider}"
    model: "{model}"

llm_preset_mapping:
  planner: default
  researcher: default
  coder: default
  auditor: default
  evaluator: default
  executor: default
  packager: default
"#,
        agents_dir = agents_dir_str,
        provider = provider,
        model = model,
    );

    std::fs::write(config_path, config_content)?;
    eprintln!("Created config at {}", config_path.display());
    Ok(())
}

fn ensure_bootstrap(config_path: &Path) -> anyhow::Result<()> {
    let config = autonoetic_gateway::config::load_config(config_path)?;
    if config.agents_dir.exists() && config.agents_dir.read_dir()?.next().is_some() {
        return Ok(());
    }
    eprintln!("Bootstrapping agents...");
    super::agent::handle_agent_bootstrap(config_path, None, false)?;
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

    ensure_config(&config_path)?;
    ensure_bootstrap(&config_path)?;

    if std::env::var("AUTONOETIC_SHARED_SECRET").is_err() {
        let secret = uuid::Uuid::new_v4().to_string();
        std::env::set_var("AUTONOETIC_SHARED_SECRET", &secret);
        eprintln!("Generated ephemeral shared secret for this session.");
    }

    let chat_args = super::common::ChatArgs {
        agent_id: args.agent_id.clone(),
        sender_id: None,
        channel_id: None,
        session_id: args.session_id.clone(),
        test_mode: false,
    };

    // Start gateway in background, then open chat
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

    // Brief pause to let the gateway bind its port
    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;

    let result = super::chat::handle_chat(&config_path, &chat_args).await;

    gateway_handle.abort();
    result
}
