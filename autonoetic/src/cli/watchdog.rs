use std::path::Path;
use std::sync::Arc;

use anyhow::Context;
use tracing::info;

use autonoetic_gateway::llm::{build_driver, Message};
use autonoetic_gateway::runtime::lifecycle::AgentExecutor;
use autonoetic_gateway::runtime::tools::default_registry;
use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_gateway::AgentRepository;

/// Run the divergence watchdog against a target session.
pub async fn handle_watchdog(config_path: &Path, session_id: &str) -> anyhow::Result<()> {
    info!(
        target: "watchdog",
        session_id = %session_id,
        "Launching divergence watchdog"
    );

    let loaded_config = autonoetic_gateway::config::load_config(config_path)?;
    let gateway_config = Arc::new(loaded_config);

    let gateway_dir = gateway_config.agents_dir.join(".gateway");
    let store = Arc::new(
        GatewayStore::open(&gateway_dir)
            .context("Failed to open GatewayStore — is the gateway running and configured?")?,
    );

    let repo = AgentRepository::from_config(&gateway_config);
    let loaded = repo
        .get_sync("watchdog.default")
        .context("Watchdog agent 'watchdog.default' not found — run with a config that points to the agents directory containing agents/specialists/watchdog/")?;
    let manifest = loaded.manifest;
    let instructions = loaded.instructions;
    let agent_dir = loaded.dir;

    let llm_config = manifest
        .llm_config
        .clone()
        .context("watchdog.default is missing llm_config in SKILL.md")?;
    let driver = build_driver(llm_config, reqwest::Client::new())?;

    let mut runtime = AgentExecutor::new(
        manifest,
        instructions,
        driver,
        agent_dir,
        default_registry(),
        Some(store),
    );
    runtime = runtime
        .with_gateway_dir(gateway_dir)
        .with_config(gateway_config)
        .with_initial_user_message(format!(
            "Review session {} for trajectory divergence patterns.\n\
             Use digest_query and execution_search to gather evidence, \
             then produce a judgment. If you find critical divergence, \
             escalate via session_escalate with high urgency.",
            session_id
        ));

    let mut history = vec![
        Message::system(runtime.instructions.clone()),
        Message::user(runtime.initial_user_message.clone()),
    ];

    match runtime.execute_with_history(&mut history).await {
        Ok(outcome) => match &outcome {
            autonoetic_gateway::runtime::lifecycle::TurnOutcome::Completed(Some(reply)) => {
                println!("{}", reply);
            }
            autonoetic_gateway::runtime::lifecycle::TurnOutcome::Completed(None) => {
                println!("[Watchdog produced no judgment]");
            }
            autonoetic_gateway::runtime::lifecycle::TurnOutcome::Suspended {
                approval_request_id,
                ..
            } => {
                println!(
                    "[Watchdog suspended for approval: {}]",
                    approval_request_id
                );
            }
            autonoetic_gateway::runtime::lifecycle::TurnOutcome::SuspendedUserInput {
                interaction_id,
            } => {
                println!("[Watchdog waiting for input: {}]", interaction_id);
            }
            other => {
                println!("[Watchdog interrupted: {:?}]", other);
            }
        },
        Err(e) => {
            eprintln!("Watchdog error: {}", e);
        }
    }

    Ok(())
}
