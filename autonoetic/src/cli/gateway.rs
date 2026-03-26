use std::path::Path;
use tracing::info;

use super::common::{
    activate_registered_mcp_servers, load_mcp_servers, mcp_registry_path, McpClient, McpTool,
    McpTransportConfig,
};

pub async fn handle_gateway_start(
    config_path: &Path,
    daemon: bool,
    port: Option<u16>,
    tls: bool,
    response_validation: Option<super::common::ResponseValidationMode>,
) -> anyhow::Result<()> {
    let mut config = autonoetic_gateway::config::load_config(config_path)?;
    super::common::apply_response_validation_override(&mut config, response_validation);
    let repo = autonoetic_gateway::AgentRepository::from_config(&config);
    let agents = repo.list().await?;
    let mcp_runtime = activate_registered_mcp_servers(config_path).await?;

    info!(
        "Gateway starting — port: {}, agents: {}, daemon: {}, tls: {}",
        port.unwrap_or(config.port),
        agents.len(),
        daemon,
        tls,
    );

    for a in &agents {
        info!("  Agent: {} ({})", a.id, a.dir.display());
    }
    for line in mcp_runtime.summary_lines() {
        info!("{}", line);
    }

    let server = autonoetic_gateway::GatewayServer::new(config);
    let _mcp_runtime = mcp_runtime;
    if let Err(e) = server.run().await {
        tracing::error!("Gateway server error: {:?}", e);
    }

    Ok(())
}

pub fn handle_gateway_stop() {
    info!("Stopping Gateway");
}

pub async fn handle_gateway_status(config_path: &Path, json_output: bool) -> anyhow::Result<()> {
    let config = autonoetic_gateway::config::load_config(config_path)?;
    let repo = autonoetic_gateway::AgentRepository::from_config(&config);
    let agents = repo.list().await?;
    let registry_path = mcp_registry_path(config_path);
    let servers = load_mcp_servers(&registry_path)?;

    let mut mcp_server_rows: Vec<(String, String, serde_json::Value, Vec<McpTool>)> =
        Vec::with_capacity(servers.len());
    for server in servers {
        let mut client = McpClient::connect(&server).await?;
        let tools = client.list_tools().await?;
        let (transport_name, transport_details) = match &server.transport {
            McpTransportConfig::Stdio => (
                "stdio".to_string(),
                serde_json::json!({
                    "type": "stdio",
                    "command": server.command,
                    "args": server.args
                }),
            ),
            McpTransportConfig::Sse { url } => (
                "sse".to_string(),
                serde_json::json!({
                    "type": "sse",
                    "url": url
                }),
            ),
        };
        mcp_server_rows.push((server.name, transport_name, transport_details, tools));
    }

    if json_output {
        let agents_json = agents
            .iter()
            .map(|agent| {
                serde_json::json!({
                    "id": agent.id,
                    "dir": agent.dir.display().to_string()
                })
            })
            .collect::<Vec<_>>();
        let mcp_servers_json = mcp_server_rows
            .iter()
            .map(|(name, _transport_name, transport_details, tools)| {
                serde_json::json!({
                    "name": name,
                    "transport": transport_details,
                    "tools_count": tools.len(),
                    "tools": tools.iter().map(|tool| serde_json::json!({
                        "name": tool.name,
                        "description": tool.description,
                        "input_schema": tool.input_schema
                    })).collect::<Vec<_>>()
                })
            })
            .collect::<Vec<_>>();

        let body = serde_json::json!({
            "gateway": {
                "config_path": config_path.display().to_string(),
                "jsonrpc_port": config.port,
                "ofp_port": config.ofp_port,
                "ofp_tls": config.tls,
                "background_scheduler_enabled": config.background_scheduler_enabled,
                "background_tick_secs": config.background_tick_secs,
                "background_min_interval_secs": config.background_min_interval_secs,
                "max_background_due_per_tick": config.max_background_due_per_tick
            },
            "agents": {
                "dir": config.agents_dir.display().to_string(),
                "count": agents.len(),
                "items": agents_json
            },
            "mcp": {
                "registry_path": registry_path.display().to_string(),
                "servers_count": mcp_server_rows.len(),
                "servers": mcp_servers_json
            }
        });
        println!("{}", serde_json::to_string_pretty(&body)?);
        return Ok(());
    }

    println!("Gateway status");
    println!(" config_path: {}", config_path.display());
    println!(" jsonrpc_port: {}", config.port);
    println!(" ofp_port: {}", config.ofp_port);
    println!(" ofp_tls: {}", config.tls);
    println!(
        " background_scheduler: enabled={}, tick_secs={}, min_interval_secs={}, max_due_per_tick={}",
        config.background_scheduler_enabled,
        config.background_tick_secs,
        config.background_min_interval_secs,
        config.max_background_due_per_tick
    );
    println!(" agents_dir: {}", config.agents_dir.display());
    println!(" agents_count: {}", agents.len());
    for agent in &agents {
        println!("  - agent: {}", agent.id);
    }

    println!(" mcp_registry_path: {}", registry_path.display());
    println!(" mcp_servers_count: {}", mcp_server_rows.len());
    for (server_name, transport_name, _transport_details, tools) in mcp_server_rows {
        println!(
            "  - mcp_server: {} (transport={}, tools={})",
            server_name,
            transport_name,
            tools.len()
        );
        for tool in tools {
            println!("      - tool: {}", tool.name);
        }
    }

    Ok(())
}

pub fn handle_gateway_approvals(
    config_path: &Path,
    command: &super::common::GatewayApprovalCommands,
) -> anyhow::Result<()> {
    let config = autonoetic_gateway::config::load_config(config_path)?;
    let gateway_dir = autonoetic_gateway::execution::gateway_root_dir(&config);
    let gateway_store =
        autonoetic_gateway::scheduler::gateway_store::GatewayStore::open(&gateway_dir)?;
    match command {
        super::common::GatewayApprovalCommands::List { json } => {
            let approvals = autonoetic_gateway::scheduler::load_approval_requests(
                &config,
                Some(&gateway_store),
            )?;
            if *json {
                println!("{}", serde_json::to_string_pretty(&approvals)?);
                return Ok(());
            }
            if approvals.is_empty() {
                println!("No pending background approval requests.");
                return Ok(());
            }
            println!(
                "{:<38} {:<20} {:<24} ACTION",
                "REQUEST ID", "AGENT", "CREATED AT"
            );
            for approval in approvals {
                println!(
                    "{:<38} {:<20} {:<24} {}",
                    approval.request_id,
                    approval.agent_id,
                    approval.created_at,
                    approval.action.kind()
                );
            }
        }
        super::common::GatewayApprovalCommands::Approve { request_id, reason } => {
            let decision = autonoetic_gateway::scheduler::approve_request(
                &config,
                Some(&gateway_store),
                request_id,
                "cli",
                reason.clone(),
            )?;
            println!(
                "Approved {} for agent {} ({})",
                decision.request_id,
                decision.agent_id,
                decision.action.kind()
            );
            println!();
            println!("The approval has been processed and a notification was queued for");
            println!("the target session. If chat is open, it should auto-resume and");
            println!("display the planner continuation without requiring a manual prompt.");
            println!();
            println!("If no chat is currently connected, the notification remains pending");
            println!("until a consumer acknowledges it.");
        }
        super::common::GatewayApprovalCommands::Reject { request_id, reason } => {
            let decision = autonoetic_gateway::scheduler::reject_request(
                &config,
                Some(&gateway_store),
                request_id,
                "cli",
                reason.clone(),
            )?;
            println!(
                "Rejected {} for agent {} ({})",
                decision.request_id,
                decision.agent_id,
                decision.action.kind()
            );
        }
    }
    Ok(())
}

pub fn handle_gateway_interactions(
    config_path: &Path,
    command: &super::common::GatewayInteractionCommands,
) -> anyhow::Result<()> {
    let config = autonoetic_gateway::config::load_config(config_path)?;
    let gateway_dir = autonoetic_gateway::execution::gateway_root_dir(&config);
    let gateway_store =
        autonoetic_gateway::scheduler::gateway_store::GatewayStore::open(&gateway_dir)?;

    match command {
        super::common::GatewayInteractionCommands::List {
            root_session,
            session,
            json,
        } => {
            let interactions = if let Some(rsid) = root_session {
                gateway_store.get_pending_interactions_for_root_session(rsid)?
            } else if let Some(sid) = session {
                gateway_store.get_pending_interactions_for_session(sid)?
            } else {
                // List all pending - use empty string to get all (or we'd need a new method)
                // For now, just show a help message
                println!("Please specify --root-session or --session to list interactions.");
                return Ok(());
            };

            if *json {
                println!("{}", serde_json::to_string_pretty(&interactions)?);
                return Ok(());
            }

            if interactions.is_empty() {
                println!("No pending user interactions.");
                return Ok(());
            }

            println!(
                "{:<14} {:<14} {:<15} {:<24} QUESTION",
                "INTERACTION ID", "AGENT", "KIND", "CREATED AT"
            );
            for i in interactions {
                let truncated_q = if i.question.len() > 60 {
                    format!("{}...", &i.question[..57])
                } else {
                    i.question.clone()
                };
                println!(
                    "{:<14} {:<14} {:<15} {:<24} {}",
                    i.interaction_id,
                    i.agent_id,
                    i.kind.as_str(),
                    i.created_at,
                    truncated_q,
                );
            }
        }
        super::common::GatewayInteractionCommands::Answer {
            interaction_id,
            text,
            option,
        } => {
            use autonoetic_types::background::UserInteractionAnswer;

            if text.is_none() && option.is_none() {
                anyhow::bail!("Must provide either --text or --option to answer an interaction");
            }
            if text.is_some() && option.is_some() {
                anyhow::bail!("Provide exactly one of --text or --option");
            }

            let answer = UserInteractionAnswer {
                interaction_id: interaction_id.clone(),
                answer_option_id: option.clone(),
                answer_text: text.clone(),
                answered_by: "cli".to_string(),
            };

            gateway_store.answer_user_interaction(&answer)?;

            println!("Answered interaction {}", interaction_id);
            if let Some(opt) = option {
                println!("  Selected option: {}", opt);
            }
            if let Some(txt) = text {
                println!("  Answer text: {}", txt);
            }
            println!();
            println!(
                "The interaction has been answered. Resume the agent session with a spawn for"
            );
            println!(
                "that session (e.g. chat/JSON-RPC `agent.spawn`), or call \
                 `GatewayExecutionService::resume_from_user_interaction` with this id."
            );
        }
        super::common::GatewayInteractionCommands::Cancel {
            interaction_id,
            reason,
        } => {
            let reason = reason
                .clone()
                .unwrap_or_else(|| "Cancelled by operator".to_string());

            gateway_store.cancel_user_interaction(interaction_id, &reason)?;

            println!("Cancelled interaction {}", interaction_id);
            println!("  Reason: {}", reason);
        }
    }

    Ok(())
}
