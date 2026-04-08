use std::io::Write;
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

pub async fn handle_gateway_approvals(
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
                "{:<38} {:<20} {:<14} {}",
                "REQUEST ID", "AGENT", "KIND", "DETAILS"
            );
            for approval in approvals {
                let details = match &approval.action {
                    autonoetic_types::background::ScheduledAction::SandboxExec {
                        command, ..
                    } => {
                        let truncated = if command.len() > 50 {
                            format!("{}...", &command[..50])
                        } else {
                            command.clone()
                        };
                        format!("exec: {}", truncated)
                    }
                    autonoetic_types::background::ScheduledAction::AgentInstall {
                        agent_id,
                        summary,
                        ..
                    } => {
                        format!("install: {} ({})", agent_id, summary)
                    }
                    other => format!("{}", other.kind()),
                };
                println!(
                    "{:<38} {:<20} {:<14} {}",
                    approval.request_id,
                    approval.agent_id,
                    approval.action.kind(),
                    details
                );
            }
        }
        super::common::GatewayApprovalCommands::Approve {
            request_id,
            reason,
            secrets,
            approval_level,
        } => {
            let approval_level = approval_level.to_runtime();
            let decision = autonoetic_gateway::scheduler::approve_request(
                &config,
                Some(&gateway_store),
                request_id,
                "cli",
                reason.clone(),
                if secrets.is_empty() {
                    None
                } else {
                    Some(secrets.clone())
                },
                Some(&approval_level),
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
        super::common::GatewayApprovalCommands::Interactive { approval_level } => {
            run_interactive_approvals(&config, &gateway_store, *approval_level).await?;
        }
    }
    Ok(())
}

async fn run_interactive_approvals(
    config: &autonoetic_types::config::GatewayConfig,
    gateway_store: &autonoetic_gateway::scheduler::gateway_store::GatewayStore,
    approval_level: super::common::CliApprovalLevel,
) -> anyhow::Result<()> {
    use autonoetic_types::background::ApprovalRequest;
    use crossterm::{
        event::{self, Event, KeyCode, KeyEventKind},
        execute,
        terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
    };
    use ratatui::{
        backend::CrosstermBackend,
        layout::{Constraint, Direction, Layout},
        style::{Color, Modifier, Style},
        text::{Line, Span},
        widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap},
        Terminal,
    };
    use std::io;

    let approvals =
        autonoetic_gateway::scheduler::load_approval_requests(config, Some(gateway_store))?;

    if approvals.is_empty() {
        println!("No pending approval requests.");
        return Ok(());
    }

    let mut items: Vec<ApprovalRequest> = approvals;
    let mut state = ListState::default();
    state.select(Some(0));
    let mut status_msg = String::new();
    let mut done = false;

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;

    while !done {
        terminal.draw(|f| {
            let area = f.area();
            if area.height < 10 || area.width < 40 {
                let p = Paragraph::new("Terminal too small");
                f.render_widget(p, area);
                return;
            }

            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(1),
                    Constraint::Min(5),
                    Constraint::Length(8),
                    Constraint::Length(2),
                ])
                .split(area);

            let title = format!(
                " Pending Approvals ({})  \u{2191}\u{2193}: navigate  a: approve  r: reject  q: quit ",
                items.len()
            );
            let title_bar = Paragraph::new(Line::from(Span::styled(
                &title,
                Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
            )));
            f.render_widget(title_bar, chunks[0]);

            let list_items: Vec<ListItem> = items
                .iter()
                .enumerate()
                .map(|(_i, req)| {
                    let (kind_label, detail) = match &req.action {
                        autonoetic_types::background::ScheduledAction::SandboxExec { command, .. } => {
                            let truncated: String = if command.len() > 60 {
                                format!("{}...", &command[..60])
                            } else {
                                command.clone()
                            };
                            ("sandbox_exec", truncated)
                        }
                        autonoetic_types::background::ScheduledAction::AgentInstall { agent_id, summary, .. } => {
                            ("agent_install", format!("{} ({})", agent_id, summary))
                        }
                        other => ("other", other.kind().to_string()),
                    };
                    let reason_str = req.reason.as_deref().unwrap_or("");

                    let line1 = Line::from(vec![
                        Span::styled(
                            format!(" {:<12} ", req.request_id),
                            Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(
                            format!("{:<20} ", req.agent_id),
                            Style::default().fg(Color::White),
                        ),
                        Span::styled(
                            format!("{:<14} ", kind_label),
                            Style::default().fg(Color::Cyan),
                        ),
                    ]);
                    let line2 = Line::from(vec![
                        Span::raw("   "),
                        Span::styled(detail.clone(), Style::default().fg(Color::Gray)),
                    ]);
                    let line3 = Line::from(vec![
                        Span::raw("   "),
                        Span::styled(
                            truncate_str(reason_str, (chunks[1].width as usize).saturating_sub(3)),
                            Style::default().fg(Color::DarkGray),
                        ),
                    ]);

                    ListItem::new(vec![line1, line2, line3])
                })
                .collect();

            let list = List::new(list_items)
                .block(Block::default().borders(Borders::NONE))
                .highlight_style(
                    Style::default()
                        .bg(Color::DarkGray)
                        .add_modifier(Modifier::BOLD),
                );
            f.render_stateful_widget(list, chunks[1], &mut state);

            let selected = state.selected();
            let detail_lines = if let Some(idx) = selected {
                let req = &items[idx];
                let mut lines = Vec::new();
                lines.push(Line::from(vec![
                    Span::styled("Request: ", Style::default().fg(Color::Gray)),
                    Span::styled(&req.request_id, Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
                ]));
                lines.push(Line::from(vec![
                    Span::styled("Agent:   ", Style::default().fg(Color::Gray)),
                    Span::styled(&req.agent_id, Style::default().fg(Color::White)),
                ]));
                lines.push(Line::from(vec![
                    Span::styled("Session: ", Style::default().fg(Color::Gray)),
                    Span::styled(&req.session_id, Style::default().fg(Color::White)),
                ]));
                lines.push(Line::from(vec![
                    Span::styled("Created: ", Style::default().fg(Color::Gray)),
                    Span::styled(&req.created_at, Style::default().fg(Color::White)),
                ]));
                if let Some(ref reason) = req.reason {
                    lines.push(Line::from(vec![
                        Span::styled("Reason:  ", Style::default().fg(Color::Gray)),
                        Span::styled(reason, Style::default().fg(Color::Magenta)),
                    ]));
                }
                match &req.action {
                    autonoetic_types::background::ScheduledAction::SandboxExec { command, dependencies, .. } => {
                        lines.push(Line::from(vec![
                            Span::styled("Command: ", Style::default().fg(Color::Gray)),
                            Span::styled(command, Style::default().fg(Color::Green)),
                        ]));
                        if let Some(deps) = dependencies {
                            let pkgs = deps.packages.join(", ");
                            lines.push(Line::from(vec![
                                Span::styled("Deps:    ", Style::default().fg(Color::Gray)),
                                Span::styled(format!("{} ({})", deps.runtime, pkgs), Style::default().fg(Color::Green)),
                            ]));
                        }
                    }
                    autonoetic_types::background::ScheduledAction::AgentInstall { agent_id, summary, payload, .. } => {
                        lines.push(Line::from(vec![
                            Span::styled("Install: ", Style::default().fg(Color::Gray)),
                            Span::styled(format!("{} ({})", agent_id, summary), Style::default().fg(Color::Green)),
                        ]));
                        if let Some(payload) = payload {
                            if let Some(caps) = payload.get("capabilities") {
                                lines.push(Line::from(vec![
                                    Span::styled("Caps:    ", Style::default().fg(Color::Gray)),
                                    Span::styled(caps.to_string(), Style::default().fg(Color::Green)),
                                ]));
                            }
                            if let Some(hosts) = payload.get("detected_network_hosts") {
                                if !hosts.as_array().map(|a| a.is_empty()).unwrap_or(true) {
                                    lines.push(Line::from(vec![
                                        Span::styled("Hosts:   ", Style::default().fg(Color::Gray)),
                                        Span::styled(hosts.to_string(), Style::default().fg(Color::Red)),
                                    ]));
                                }
                            }
                        }
                    }
                    other => {
                        lines.push(Line::from(vec![
                            Span::styled("Action:  ", Style::default().fg(Color::Gray)),
                            Span::styled(other.kind(), Style::default().fg(Color::White)),
                        ]));
                    }
                }
                lines
            } else {
                vec![Line::from(Span::styled(
                    "No selection",
                    Style::default().fg(Color::DarkGray),
                ))]
            };

            let detail = Paragraph::new(detail_lines)
                .block(Block::default().borders(Borders::TOP).border_style(Style::default().fg(Color::DarkGray)))
                .wrap(Wrap { trim: false });
            f.render_widget(detail, chunks[2]);

            let status = if status_msg.is_empty() {
                Line::from(Span::styled(
                    " \u{2191}\u{2193}/j/k: navigate  a: approve  r: reject  R: refresh  q: quit",
                    Style::default().fg(Color::DarkGray),
                ))
            } else {
                Line::from(Span::styled(
                    format!(" {}", status_msg),
                    Style::default().fg(Color::Green),
                ))
            };
            let status_bar = Paragraph::new(status);
            f.render_widget(status_bar, chunks[3]);
        })?;

        if event::poll(std::time::Duration::from_millis(100))? {
            if let Event::Key(key) = event::read()? {
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                match key.code {
                    KeyCode::Char('q') | KeyCode::Esc => {
                        done = true;
                    }
                    KeyCode::Up | KeyCode::Char('k') => {
                        let selected = state.selected().unwrap_or(0);
                        if selected > 0 {
                            state.select(Some(selected - 1));
                        }
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        let selected = state.selected().unwrap_or(0);
                        if selected < items.len() - 1 {
                            state.select(Some(selected + 1));
                        }
                    }
                    KeyCode::Char('a') => {
                        if let Some(idx) = state.selected() {
                            let req = &items[idx];

                            // For CredentialPrompt, prompt for secrets interactively
                            let secrets =
                                if let autonoetic_types::background::ScheduledAction::CredentialPrompt {
                                    service,
                                    secret_fields,
                                    ..
                                } = &req.action
                                {
                                    println!("\nCredential setup for '{}' requires secret input:", service);
                                    let mut collected = Vec::new();
                                    let mut password_read_failed = false;
                                    for field in secret_fields {
                                        print!(
                                            "  {} ({}): ",
                                            field.label,
                                            if field.masked { "masked" } else { "visible" }
                                        );
                                        std::io::stdout().flush().ok();
                                        let mut input = String::new();
                                        if field.masked {
                                            // Read without echo
                                            let pass = rpassword::prompt_password("");
                                            match pass {
                                                Ok(v) => input = v,
                                                Err(_) => {
                                                    password_read_failed = true;
                                                    break;
                                                }
                                            }
                                        } else {
                                            std::io::stdin().read_line(&mut input).ok();
                                            input = input.trim().to_string();
                                        }
                                        collected.push((field.name.clone(), input));
                                    }
                                    if collected.len() != secret_fields.len() {
                                        status_msg = if password_read_failed {
                                            "Failed to read password".to_string()
                                        } else {
                                            "Secret input cancelled".to_string()
                                        };
                                        continue;
                                    }
                                    Some(collected)
                                } else {
                                    None
                                };

                            match autonoetic_gateway::scheduler::approve_request(
                                config,
                                Some(gateway_store),
                                &req.request_id,
                                "cli-interactive",
                                None,
                                secrets,
                                Some(&approval_level.to_runtime()),
                            ) {
                                Ok(decision) => {
                                    status_msg = format!(
                                        "\u{2705} Approved {} ({})",
                                        decision.request_id,
                                        decision.action.kind()
                                    );
                                    items.remove(idx);
                                    if items.is_empty() {
                                        done = true;
                                        status_msg.push_str("  \u{2014} All approvals resolved!");
                                    } else if idx >= items.len() {
                                        state.select(Some(items.len() - 1));
                                    } else {
                                        state.select(Some(idx));
                                    }
                                }
                                Err(e) => {
                                    status_msg = format!("\u{274c} Error: {}", e);
                                }
                            }
                        }
                    }
                    KeyCode::Char('r') => {
                        if let Some(idx) = state.selected() {
                            let req = &items[idx];
                            match autonoetic_gateway::scheduler::reject_request(
                                config,
                                Some(gateway_store),
                                &req.request_id,
                                "cli-interactive",
                                None,
                            ) {
                                Ok(decision) => {
                                    status_msg = format!(
                                        "\u{274c} Rejected {} ({})",
                                        decision.request_id,
                                        decision.action.kind()
                                    );
                                    items.remove(idx);
                                    if items.is_empty() {
                                        done = true;
                                        status_msg.push_str("  \u{2014} All approvals resolved!");
                                    } else if idx >= items.len() {
                                        state.select(Some(items.len() - 1));
                                    } else {
                                        state.select(Some(idx));
                                    }
                                }
                                Err(e) => {
                                    status_msg = format!("\u{274c} Error: {}", e);
                                }
                            }
                        }
                    }
                    KeyCode::Char('R') => {
                        match autonoetic_gateway::scheduler::load_approval_requests(
                            config,
                            Some(gateway_store),
                        ) {
                            Ok(refreshed) => {
                                items = refreshed;
                                if items.is_empty() {
                                    done = true;
                                    status_msg = "No more pending approvals.".to_string();
                                } else {
                                    state.select(Some(0));
                                    status_msg =
                                        format!("Refreshed: {} pending approval(s)", items.len());
                                }
                            }
                            Err(e) => {
                                status_msg = format!("Refresh error: {}", e);
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    if !status_msg.is_empty() {
        println!("{}", status_msg);
    }
    Ok(())
}

fn truncate_str(s: &str, max_len: usize) -> String {
    if s.len() <= max_len {
        s.to_string()
    } else {
        format!("{}...", &s[..max_len.saturating_sub(3)])
    }
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
