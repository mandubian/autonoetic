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

    let caps = autonoetic_gateway::host_capabilities::HostCapabilities::gather();
    for line in caps.summary_lines() {
        info!("{}", line);
    }
    if !caps.has_any_sandbox_tier() {
        tracing::warn!(
            "No sandbox tier is runnable on this host (no bwrap/docker/firecracker, no wasm-tier build) — \
             agents will fail to spawn. Run `autonoetic gateway preflight` for details."
        );
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

/// Probe and report host capabilities (sandbox tiers + language toolchains).
/// Exit code is non-zero when no sandbox tier is runnable at all.
pub fn handle_gateway_preflight(json: bool) -> anyhow::Result<()> {
    let caps = autonoetic_gateway::host_capabilities::HostCapabilities::gather();
    if json {
        println!("{}", serde_json::to_string_pretty(&caps)?);
    } else {
        for line in caps.summary_lines() {
            println!("{line}");
        }
    }
    if !caps.has_any_sandbox_tier() {
        anyhow::bail!(
            "No sandbox tier is runnable on this host — install bwrap, docker, or firecracker, or build with the wasm-tier feature."
        );
    }
    Ok(())
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
                    autonoetic_types::background::ScheduledAction::SessionEscalate {
                        reason,
                        urgency,
                        ..
                    } => {
                        let truncated = if reason.len() > 40 {
                            format!("{}...", &reason[..40])
                        } else {
                            reason.clone()
                        };
                        format!("escalation ({}): {}", urgency, truncated)
                    }
                    autonoetic_types::background::ScheduledAction::RevisionPromote {
                        agent_id,
                        revision_id,
                        added_capabilities,
                        broadened_capabilities,
                        ..
                    } => {
                        let mut all = added_capabilities.clone();
                        all.extend(broadened_capabilities.iter().cloned());
                        format!(
                            "promote {}@{}: +[{}]",
                            agent_id,
                            revision_id,
                            all.join(", ")
                        )
                    }
                    other => format!("{}", other.kind()),
                };
                println!(
                    "{:<38} {:<20} {:<14} {}{}",
                    approval.request_id,
                    approval.agent_id,
                    approval.action.kind(),
                    details,
                    if let (Some(ref sim_id), Some(ref score)) =
                        (approval.similar_to_request_id, approval.similarity_score)
                    {
                        format!(" ~{} ({:.0}%)", &sim_id[..sim_id.len().min(12)], score * 100.0)
                    } else {
                        String::new()
                    }
                );
            }
        }
        super::common::GatewayApprovalCommands::Approve {
            request_id,
            reason,
            secrets,
            approval_level,
            scope,
            targets,
            ttl,
            until,
            acknowledge_capabilities,
            confirm_phrase,
        } => {
            let approval_level = approval_level.to_runtime();
            let grant_scope = scope.to_runtime();

            let parsed_targets: Vec<autonoetic_types::background::GrantTarget> = if targets.is_empty() {
                vec![]
            } else {
                targets.iter().map(|s| super::common::parse_grant_target_spec(s))
                    .collect::<Result<Vec<_>, _>>()?
            };

            let expires_at = if let Some(ttl) = ttl {
                if until.is_some() {
                    anyhow::bail!("--ttl and --until are mutually exclusive; provide one or the other");
                }
                Some(super::common::parse_ttl(&ttl)?)
            } else if let Some(ref until_str) = until {
                let _ = chrono::DateTime::parse_from_rfc3339(until_str)
                    .map_err(|e| anyhow::anyhow!("invalid --until timestamp (expected RFC3339): {}", e))?;
                until.clone()
            } else {
                None
            };

            let decision = autonoetic_gateway::scheduler::approve_request_with_options(
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
                None,
                autonoetic_gateway::scheduler::ApproveOptions {
                    grant_scope: Some(grant_scope),
                    grant_targets: parsed_targets,
                    grant_expires_at: expires_at,
                    acknowledged_capabilities: acknowledge_capabilities.clone(),
                    confirm_phrase: confirm_phrase.clone(),
                },
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
                None,
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
        super::common::GatewayApprovalCommands::Show { request_id } => {
            let approval = gateway_store.get_approval(request_id)?;
            match approval {
                None => println!("Approval '{}' not found.", request_id),
                Some(a) => {
                    println!("Request ID:    {}", a.request_id);
                    println!("Agent:         {}", a.agent_id);
                    println!("Session:       {}", a.session_id);
                    println!("Status:        {}", a.status.as_ref().map(|s| match s {
                        autonoetic_types::background::ApprovalStatus::Approved => "approved",
                        autonoetic_types::background::ApprovalStatus::Rejected => "rejected",
                        autonoetic_types::background::ApprovalStatus::Cancelled => "cancelled",
                    }).unwrap_or("pending"));
                    println!("Created:       {}", a.created_at);
                    if let Some(ref at) = a.decided_at { println!("Decided at:    {}", at); }
                    if let Some(ref by) = a.decided_by { println!("Decided by:    {}", by); }
                    if let Some(ref r) = a.reason { println!("Reason:        {}", r); }
                    if let Some(ref r) = a.decision_reason { println!("Decision note: {}", r); }

                    match &a.action {
                        autonoetic_types::background::ScheduledAction::SandboxExec {
                            command, detected_hosts, dependencies, ..
                        } => {
                            println!("\nAction: sandbox_exec");
                            println!("  Command: {}", command);
                            if let Some(hosts) = detected_hosts {
                                if !hosts.is_empty() {
                                    println!("  Hosts:   {}", hosts.join(", "));
                                }
                            }
                            if let Some(deps) = dependencies {
                                println!("  Runtime: {}", deps.runtime);
                                if !deps.packages.is_empty() {
                                    println!("  Packages: {}", deps.packages.join(", "));
                                }
                            }
                        }
                        autonoetic_types::background::ScheduledAction::RevisionPromote {
                            agent_id,
                            revision_id,
                            outgoing_revision_id,
                            added_capabilities,
                            broadened_capabilities,
                            ..
                        } => {
                            println!("\nAction: revision_promote (R++2 capability accretion)");
                            println!("  Agent:           {}", agent_id);
                            println!("  Outgoing:        {}", outgoing_revision_id);
                            println!("  Incoming:        {}", revision_id);
                            if !added_capabilities.is_empty() {
                                println!("  Added caps:      {}", added_capabilities.join(", "));
                            }
                            if !broadened_capabilities.is_empty() {
                                println!("  Broadened caps:  {}", broadened_capabilities.join(", "));
                            }
                            println!(
                                "  Approve with:    --acknowledge-capability {}",
                                added_capabilities
                                    .iter()
                                    .chain(broadened_capabilities.iter())
                                    .cloned()
                                    .collect::<Vec<_>>()
                                    .join(" --acknowledge-capability ")
                            );
                        }
                        other => println!("\nAction: {}", other.kind()),
                    }

                    if let (Some(ref sim_id), Some(ref score)) =
                        (a.similar_to_request_id, a.similarity_score)
                    {
                        println!("\nSimilar to: ~{} ({:.0}%)", sim_id, score * 100.0);
                        let sim_approval = gateway_store.get_approval(sim_id)?;
                        if let Some(sa) = &sim_approval {
                            let status_str = sa.status.as_ref().map(|s| match s {
                                autonoetic_types::background::ApprovalStatus::Approved => "approved",
                                autonoetic_types::background::ApprovalStatus::Rejected => "rejected",
                                autonoetic_types::background::ApprovalStatus::Cancelled => "cancelled",
                            }).unwrap_or("pending");
                            println!("  Similar approval was: {}", status_str);
                        }
                    }

                    if let Some(dwell) = a.min_dwell_ms {
                        println!("\nR++4 min dwell: {} ms", dwell);
                    }
                    if let Some(ref phrase) = a.confirm_phrase {
                        println!("R++4 confirm:   --confirm-phrase '{}'", phrase);
                    }

                    if let Ok(msgs) = gateway_store.get_gate_messages(request_id) {
                        if !msgs.is_empty() {
                            println!("\nEnrichment:");
                            for msg in &msgs {
                                let ts: String = msg.created_at.chars().take(19).collect();
                                for (i, ln) in msg.content.lines().enumerate() {
                                    if i == 0 {
                                        println!("  [{}] {}: {}", ts, msg.sender, ln);
                                    } else {
                                        println!("  {}{}", " ".repeat(22 + msg.sender.len()), ln);
                                    }
                                }
                            }
                        }
                    }

                    // Code excerpts for operator inspection.
                    if let Some(ref excerpts) = a.code_excerpts {
                        println!("\n--- Code Excerpts ({} file(s)) ---", excerpts.len());
                        for exc in excerpts {
                            println!("  File: {}  ({} bytes{})",
                                exc.file_name,
                                exc.size_bytes,
                                if exc.truncated {
                                    format!(", truncated from {}", exc.truncated_from_bytes.unwrap_or(0))
                                } else { String::new() },
                            );
                            if exc.content.len() <= 1500 {
                                println!("  ```{}", exc.language);
                                for ln in exc.content.lines() {
                                    println!("  {}", ln);
                                }
                                println!("  ```");
                            } else {
                                println!("  (content too large, use the interactive TUI to view)");
                            }
                            println!();
                        }
                    }
                    if let Some(ref risk) = a.risk_summary {
                        println!("--- Risk Summary ---");
                        if risk.host_count > 0 {
                            println!("  Remote hosts: {}", risk.host_count);
                        }
                        if !risk.protocol_mix.is_empty() {
                            println!("  Protocols: {}", risk.protocol_mix.join(", "));
                        }
                        if !risk.dangerous_patterns.is_empty() {
                            println!("  Dangerous patterns:");
                            for p in &risk.dangerous_patterns {
                                println!("    - {}", p);
                            }
                        }
                        if let Some(ref v) = risk.auditor_verdict {
                            println!("  Auditor verdict: {}", v);
                        }
                        if let Some(ref link) = risk.auditor_findings_link {
                            println!("  Auditor findings: {}", link);
                        }
                    }
                }
            }
        }
        super::common::GatewayApprovalCommands::Ask {
            request_id,
            question,
        } => {
            let approval = gateway_store.get_approval(request_id)?;
            match approval {
                None => println!("Approval '{}' not found.", request_id),
                Some(a) => {
                    println!("Q: {}", question);
                    println!();
                    match ask_approval_question_llm(&a, question, &config).await {
                        Ok(answer) => println!("{}", answer),
                        Err(e) => eprintln!("Error: {e}"),
                    }
                }
            }
        }
        super::common::GatewayApprovalCommands::Comment {
            request_id,
            message,
        } => {
            if message.trim().is_empty() {
                eprintln!("Error: message must not be empty");
                return Ok(());
            }
            if gateway_store.get_approval(request_id)?.is_none() {
                eprintln!("Approval '{}' not found.", request_id);
                return Ok(());
            }
            let redacted = autonoetic_gateway::log_redaction::redact_text_for_logs(message);
            let id = gateway_store.add_gate_message(request_id, "operator", &redacted)?;
            println!(
                "Posted comment #{} on approval {}. Visible to the agent via approval.status.",
                id, request_id
            );
        }
        super::common::GatewayApprovalCommands::AskAgent {
            request_id,
            question,
        } => {
            if question.trim().is_empty() {
                eprintln!("Error: question must not be empty");
                return Ok(());
            }
            if gateway_store.get_approval(request_id)?.is_none() {
                eprintln!("Approval '{}' not found.", request_id);
                return Ok(());
            }
            println!("Q: {}", question);
            println!();
            let gateway_store_arc = std::sync::Arc::new(
                autonoetic_gateway::scheduler::gateway_store::GatewayStore::open(&gateway_dir)?,
            );
            let service = autonoetic_gateway::execution::GatewayExecutionService::new(
                config.clone(),
                Some(gateway_store_arc),
            );
            match service
                .spawn_clarification_for_approval(request_id.trim(), question.trim())
                .await
            {
                Ok(outcome) => {
                    println!("{}", outcome.answer);
                    println!();
                    println!("(child session: {})", outcome.child_session_id);
                }
                Err(e) => eprintln!("Error: {}", e),
            }
        }
        super::common::GatewayApprovalCommands::Stats { agent, session, since } => {
            let since_ts = since.as_ref().map(|s| {
                let secs = if s.ends_with('h') {
                    s.trim_end_matches('h').parse::<i64>().unwrap_or(1) * 3600
                } else if s.ends_with('d') {
                    s.trim_end_matches('d').parse::<i64>().unwrap_or(1) * 86400
                } else if s.ends_with('m') {
                    s.trim_end_matches('m').parse::<i64>().unwrap_or(1) * 60
                } else {
                    s.parse::<i64>().unwrap_or(3600)
                };
                (chrono::Utc::now() - chrono::Duration::seconds(secs)).to_rfc3339()
            });
            let stats = gateway_store.get_approval_stats(
                agent.as_deref(),
                session.as_deref(),
                since_ts.as_deref(),
            )?;
            println!("Approval Statistics");
            if agent.is_some() || session.is_some() || since.is_some() {
                let mut filters = Vec::new();
                if let Some(ref a) = agent { filters.push(format!("agent={}", a)); }
                if let Some(ref s) = session { filters.push(format!("session={}", s)); }
                if let Some(ref s) = since { filters.push(format!("since={}", s)); }
                println!("  Filters: {}", filters.join(", "));
            }
            println!();
            println!("  Total:          {}", stats["total"].as_i64().unwrap_or(0));
            println!("  Approved:       {}", stats["approved"].as_i64().unwrap_or(0));
            println!("  Rejected:       {}", stats["rejected"].as_i64().unwrap_or(0));
            println!("  Pending:        {}", stats["pending"].as_i64().unwrap_or(0));
            println!("  Approval rate:  {}", stats["approval_rate"].as_str().unwrap_or("N/A"));
            println!("  Rejection rate: {}", stats["rejection_rate"].as_str().unwrap_or("N/A"));
            if let Some(top_agents) = stats["top_agents"].as_array() {
                if !top_agents.is_empty() {
                    println!();
                    println!("  Top agents:");
                    for entry in top_agents {
                        let aid = entry["agent_id"].as_str().unwrap_or("?");
                        let cnt = entry["count"].as_i64().unwrap_or(0);
                        println!("    {:<30} {}", aid, cnt);
                    }
                }
            }
        }
    }
    Ok(())
}

pub async fn handle_gateway_grants(
    config_path: &Path,
    command: &super::common::GatewayGrantCommands,
) -> anyhow::Result<()> {
    use super::common::GatewayGrantCommands;

    let config = autonoetic_gateway::config::load_config(config_path)?;
    let gateway_dir = autonoetic_gateway::execution::gateway_root_dir(&config);
    let gateway_store = autonoetic_gateway::scheduler::gateway_store::GatewayStore::open(&gateway_dir)?;

    match command {
        GatewayGrantCommands::List { root_session, json } => {
            let grants = gateway_store.get_session_grants_structured(root_session)?;
            if *json {
                println!("{}", serde_json::to_string_pretty(&grants)?);
            } else if grants.is_empty() {
                println!("No active grants for session {}", root_session);
            } else {
                println!("{:<6} {:<36} {:<14} {:<12} {:<12} {}",
                    "ID", "SESSION", "AGENT", "SCOPE", "EXPIRES", "TARGETS");
                for g in &grants {
                    let targets_str: Vec<String> = g.targets.iter().map(|t| match t {
                        autonoetic_types::background::GrantTarget::ExactHost(h) => h.clone(),
                        autonoetic_types::background::GrantTarget::HostSuffix(s) => format!("*.{}", s),
                        autonoetic_types::background::GrantTarget::HostAndPort { host, port } => format!("{}:{}", host, port),
                        autonoetic_types::background::GrantTarget::UrlPrefix(u) => u.clone(),
                    }).collect();
                    let expires = g.expires_at.as_deref().unwrap_or("never");
                    let short_session = if g.session_id.len() > 34 {
                        format!("{}...", &g.session_id[..34])
                    } else {
                        g.session_id.clone()
                    };
                    println!("{:<6} {:<36} {:<14} {:<12} {:<12} {}",
                        g.id,
                        short_session,
                        g.agent_id,
                        match g.scope {
                            autonoetic_types::background::GrantScope::RootSession => "root",
                            autonoetic_types::background::GrantScope::Session => "session",
                        },
                        expires,
                        targets_str.join(", ")
                    );
                }
            }
        }
        GatewayGrantCommands::Revoke { root_session, host, all, reason } => {
            if host.is_none() && !all {
                anyhow::bail!("Specify --host <host> or --all to revoke grants");
            }
            let reason_text = reason.as_deref().unwrap_or("Revoked by operator");
            let count = gateway_store.revoke_session_grants(
                root_session,
                host.as_deref(),
                reason_text,
            )?;
            if count == 0 {
                println!("No matching grants found for session {}", root_session);
            } else {
                println!("Revoked {} grant(s) for session {} (reason: {})", count, root_session, reason_text);
                if let Some(ref host_val) = host {
                    println!("  Host filter: {}", host_val);
                }
            }

            gateway_store.create_causal_event(&autonoetic_types::causal_chain::CausalEventRecord {
                event_id: format!("grant-revoke-{}", uuid::Uuid::new_v4()),
                agent_id: "gateway".to_string(),
                session_id: root_session.clone(),
                turn_id: None,
                event_seq: 0,
                timestamp: chrono::Utc::now().to_rfc3339(),
                category: "grant_revocation".to_string(),
                action: "revoke_grants".to_string(),
                status: "completed".to_string(),
                enforced_rules: autonoetic_types::causal_chain::default_enforced_rules(),
                target: host.clone(),
                payload: Some(serde_json::json!({
                    "reason": reason_text,
                    "count": count,
                }).to_string()),
                payload_ref: None,
                evidence_ref: None,
                reason: reason.clone(),
            })?;
        }
    }
    Ok(())
}

pub async fn handle_gateway_exec_cache(
    config_path: &Path,
    command: &super::common::GatewayExecCacheCommands,
) -> anyhow::Result<()> {
    use super::common::GatewayExecCacheCommands;

    let config = autonoetic_gateway::config::load_config(config_path)?;
    let gateway_dir = autonoetic_gateway::execution::gateway_root_dir(&config);
    let cache =
        autonoetic_gateway::runtime::approved_exec_cache::ApprovedExecCache::new(&gateway_dir)?;

    match command {
        GatewayExecCacheCommands::List { json } => {
            let entries = cache.all();
            if *json {
                println!("{}", serde_json::to_string_pretty(&entries)?);
            } else if entries.is_empty() {
                println!("No cached exec approvals.");
            } else {
                println!(
                    "{:<22} {:<18} {:<20} {:<12} {}",
                    "FINGERPRINT", "AGENT", "APPROVED_AT", "BY", "TARGETS"
                );
                for e in &entries {
                    // Show a short, copyable prefix of the fingerprint.
                    let short_fp = e.fingerprint.chars().take(21).collect::<String>();
                    let approved = e.approved_at.chars().take(19).collect::<String>();
                    println!(
                        "{:<22} {:<18} {:<20} {:<12} {}",
                        short_fp,
                        e.agent_id,
                        approved,
                        e.approved_by,
                        e.remote_targets.join(", ")
                    );
                }
                println!(
                    "\n{} entr{}. Revoke with: gateway exec-cache revoke <fingerprint>",
                    entries.len(),
                    if entries.len() == 1 { "y" } else { "ies" }
                );
            }
        }
        GatewayExecCacheCommands::Revoke {
            fingerprint,
            all,
            reason,
        } => {
            if fingerprint.is_none() && !all {
                anyhow::bail!("Specify a <fingerprint> or --all to revoke exec-cache approvals");
            }
            let reason_text = reason.as_deref().unwrap_or("Revoked by operator");

            let (count, target) = if *all {
                (cache.clear()?, None)
            } else {
                let fp = fingerprint.as_deref().unwrap();
                // Accept a full `sha256:…` or a unique prefix copied from `list`.
                let matches: Vec<String> = cache
                    .all()
                    .into_iter()
                    .map(|e| e.fingerprint)
                    .filter(|f| f == fp || f.starts_with(fp))
                    .collect();
                match matches.len() {
                    0 => {
                        println!("No cached exec approval matching '{}'.", fp);
                        return Ok(());
                    }
                    1 => {
                        let full = &matches[0];
                        let removed = cache.remove(full)?;
                        (usize::from(removed), Some(full.clone()))
                    }
                    n => {
                        anyhow::bail!(
                            "'{}' matches {} entries — use the full fingerprint from `exec-cache list`",
                            fp,
                            n
                        );
                    }
                }
            };

            if count == 0 {
                println!("No matching exec-cache approvals revoked.");
                return Ok(());
            }
            println!(
                "Revoked {} cached exec approval(s) (reason: {}). The next matching exec will require fresh approval.",
                count, reason_text
            );

            // Best-effort audit event.
            if let Ok(store) =
                autonoetic_gateway::scheduler::gateway_store::GatewayStore::open(&gateway_dir)
            {
                let _ = store.create_causal_event(&autonoetic_types::causal_chain::CausalEventRecord {
                    event_id: format!("exec-cache-revoke-{}", uuid::Uuid::new_v4()),
                    agent_id: "gateway".to_string(),
                    session_id: "operator".to_string(),
                    turn_id: None,
                    event_seq: 0,
                    timestamp: chrono::Utc::now().to_rfc3339(),
                    category: "exec_cache_revocation".to_string(),
                    action: if *all { "revoke_all" } else { "revoke" }.to_string(),
                    status: "completed".to_string(),
                    enforced_rules: autonoetic_types::causal_chain::default_enforced_rules(),
                    target: target.clone(),
                    payload: Some(
                        serde_json::json!({ "reason": reason_text, "count": count }).to_string(),
                    ),
                    payload_ref: None,
                    evidence_ref: None,
                    reason: reason.clone(),
                });
            }
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
    let mut show_code_fullscreen = false;
    state.select(Some(0));
    let mut status_msg = String::new();
    let mut done = false;

    // Input mode state
    #[derive(PartialEq)]
    enum TuiMode {
        Navigate,
        AskQuestion,
        WriteMessage,
        AskAgent,
    }
    let mut tui_mode = TuiMode::Navigate;
    let mut question_input = String::new();
    let mut question_answer = String::new();
    let mut message_input = String::new();
    let mut ask_agent_input = String::new();
    let mut ask_agent_status = String::new();

    let mut enrichment_cache: std::collections::HashMap<String, Vec<autonoetic_gateway::runtime::human_gate::GateMessage>> =
        std::collections::HashMap::new();
    let mut last_selected: Option<usize> = None;

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
                " Pending Approvals ({})  \u{2191}\u{2193}: navigate  a: approve  r: reject  ?: ask  m: note  A: ask-agent  c: code  q: quit ",
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
                        autonoetic_types::background::ScheduledAction::SessionEscalate { urgency, reason, .. } => {
                            let truncated: String = if reason.len() > 40 {
                                format!("{}...", &reason[..40])
                            } else {
                                reason.clone()
                            };
                            ("escalation", format!("[{}] {}", urgency, truncated))
                        }
                        autonoetic_types::background::ScheduledAction::RevisionPromote {
                            agent_id,
                            revision_id,
                            added_capabilities,
                            broadened_capabilities,
                            ..
                        } => {
                            let mut all = added_capabilities.clone();
                            all.extend(broadened_capabilities.iter().cloned());
                            (
                                "promote",
                                format!("{}@{}: +[{}]", agent_id, revision_id, all.join(", ")),
                            )
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

            // In Q&A mode, the detail panel shows the answer (or a prompt).
            let detail_lines = if tui_mode == TuiMode::AskQuestion {
                if question_answer.is_empty() {
                    vec![Line::from(Span::styled(
                        " Type your question about this approval and press Enter…",
                        Style::default().fg(Color::DarkGray),
                    ))]
                } else {
                    let mut lines = vec![Line::from(vec![
                        Span::styled(" Q: ", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
                        Span::styled(question_input.clone(), Style::default().fg(Color::White)),
                    ])];
                    for answer_line in question_answer.lines() {
                        lines.push(Line::from(Span::styled(
                            format!(" {}", answer_line),
                            Style::default().fg(Color::Yellow),
                        )));
                    }
                    lines
                }
            } else if tui_mode == TuiMode::WriteMessage {
                vec![Line::from(Span::styled(
                    " Type a note to append to this approval's enrichment thread, then press Enter…",
                    Style::default().fg(Color::DarkGray),
                ))]
            } else if tui_mode == TuiMode::AskAgent {
                if ask_agent_status.is_empty() {
                    vec![Line::from(Span::styled(
                        " Type a question for the agent that requested this approval, then press Enter…\n\
                         (Spawns a read-only clarification child of the agent — see ask-agent in docs.)",
                        Style::default().fg(Color::DarkGray),
                    ))]
                } else {
                    let mut lines = vec![Line::from(vec![
                        Span::styled(" Q: ", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
                        Span::styled(ask_agent_input.clone(), Style::default().fg(Color::White)),
                    ])];
                    for ans_line in ask_agent_status.lines() {
                        lines.push(Line::from(Span::styled(
                            format!(" {}", ans_line),
                            Style::default().fg(Color::Yellow),
                        )));
                    }
                    lines
                }
            } else if let Some(idx) = selected {
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
                let redacted_action = req.action.redact_for_display();
                match &redacted_action {
                    autonoetic_types::background::ScheduledAction::SandboxExec { command, dependencies, .. } => {
                        let command = command.clone();
                        let deps = dependencies.clone();
                        lines.push(Line::from(vec![
                            Span::styled("Command: ", Style::default().fg(Color::Gray)),
                            Span::styled(command, Style::default().fg(Color::Green)),
                        ]));
                        if let Some(deps) = deps {
                            let pkgs = deps.packages.join(", ");
                            lines.push(Line::from(vec![
                                Span::styled("Deps:    ", Style::default().fg(Color::Gray)),
                                Span::styled(format!("{} ({})", deps.runtime, pkgs), Style::default().fg(Color::Green)),
                            ]));
                        }
                    }
                    autonoetic_types::background::ScheduledAction::AgentInstall { agent_id, summary, payload, .. } => {
                        let agent_id = agent_id.clone();
                        let summary = summary.clone();
                        let payload = payload.clone();
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
                    autonoetic_types::background::ScheduledAction::SessionEscalate {
                        reason, context, urgency, suggested_actions, ..
                    } => {
                        let reason = reason.clone();
                        let context = context.clone();
                        let urgency = urgency.clone();
                        let suggested_actions = suggested_actions.clone();
                        lines.push(Line::from(vec![
                            Span::styled("Urgency: ", Style::default().fg(Color::Gray)),
                            Span::styled(urgency, Style::default().fg(Color::Yellow)),
                        ]));
                        lines.push(Line::from(vec![
                            Span::styled("Reason:  ", Style::default().fg(Color::Gray)),
                            Span::styled(reason, Style::default().fg(Color::Green)),
                        ]));
                        if !context.is_empty() {
                            lines.push(Line::from(vec![
                                Span::styled("Context: ", Style::default().fg(Color::Gray)),
                                Span::styled(context, Style::default().fg(Color::Green)),
                            ]));
                        }
                        if !suggested_actions.is_empty() {
                            lines.push(Line::from(vec![
                                Span::styled("Actions: ", Style::default().fg(Color::Gray)),
                                Span::styled(suggested_actions.join("; "), Style::default().fg(Color::Green)),
                            ]));
                        }
                    }
                    autonoetic_types::background::ScheduledAction::RevisionPromote {
                        agent_id,
                        revision_id,
                        outgoing_revision_id,
                        added_capabilities,
                        broadened_capabilities,
                        ..
                    } => {
                        let agent_id = agent_id.clone();
                        let revision_id = revision_id.clone();
                        let outgoing_revision_id = outgoing_revision_id.clone();
                        let added_capabilities = added_capabilities.clone();
                        let broadened_capabilities = broadened_capabilities.clone();
                        lines.push(Line::from(vec![
                            Span::styled("Promote: ", Style::default().fg(Color::Gray)),
                            Span::styled(
                                format!("{} → {}", outgoing_revision_id, revision_id),
                                Style::default().fg(Color::Yellow),
                            ),
                        ]));
                        lines.push(Line::from(vec![
                            Span::styled("Agent:   ", Style::default().fg(Color::Gray)),
                            Span::styled(agent_id, Style::default().fg(Color::Green)),
                        ]));
                        if !added_capabilities.is_empty() {
                            lines.push(Line::from(vec![
                                Span::styled("Added:   ", Style::default().fg(Color::Gray)),
                                Span::styled(
                                    added_capabilities.join(", "),
                                    Style::default().fg(Color::Red),
                                ),
                            ]));
                        }
                        if !broadened_capabilities.is_empty() {
                            lines.push(Line::from(vec![
                                Span::styled("Broaden: ", Style::default().fg(Color::Gray)),
                                Span::styled(
                                    broadened_capabilities.join(", "),
                                    Style::default().fg(Color::Red),
                                ),
                            ]));
                        }
                        lines.push(Line::from(vec![
                            Span::styled("Note:    ", Style::default().fg(Color::Gray)),
                            Span::styled(
                                "approve from CLI with --acknowledge-capability for each (R++2)",
                                Style::default().fg(Color::DarkGray),
                            ),
                        ]));
                    }
                    autonoetic_types::background::ScheduledAction::CredentialRequest {
                        credential_id, url, method, headers, body, inject_secret_as, ..
                    } => {
                        let credential_id = credential_id.clone();
                        let url = url.clone();
                        let method = method.clone();
                        let headers = headers.clone();
                        let body = body.clone();
                        let inject_secret_as = inject_secret_as.clone();
                        lines.push(Line::from(vec![
                            Span::styled("Type:    ", Style::default().fg(Color::Gray)),
                            Span::styled("credential_request", Style::default().fg(Color::White)),
                        ]));
                        lines.push(Line::from(vec![
                            Span::styled("Cred:    ", Style::default().fg(Color::Gray)),
                            Span::styled(credential_id, Style::default().fg(Color::Green)),
                        ]));
                        lines.push(Line::from(vec![
                            Span::styled("URL:     ", Style::default().fg(Color::Gray)),
                            Span::styled(url, Style::default().fg(Color::Green)),
                        ]));
                        if let Some(m) = method {
                            lines.push(Line::from(vec![
                                Span::styled("Method:  ", Style::default().fg(Color::Gray)),
                                Span::styled(m, Style::default().fg(Color::Green)),
                            ]));
                        }
                        if let Some(h) = headers {
                            if !h.is_empty() {
                                lines.push(Line::from(vec![
                                    Span::styled("Headers: ", Style::default().fg(Color::Gray)),
                                    Span::styled(
                                        serde_json::to_string(&h).unwrap_or_default(),
                                        Style::default().fg(Color::Green),
                                    ),
                                ]));
                            }
                        }
                        if let Some(b) = body {
                            lines.push(Line::from(vec![
                                Span::styled("Body:    ", Style::default().fg(Color::Gray)),
                                Span::styled(
                                    serde_json::to_string(&b).unwrap_or_default(),
                                    Style::default().fg(Color::Green),
                                ),
                            ]));
                        }
                        if let Some(i) = inject_secret_as {
                            lines.push(Line::from(vec![
                                Span::styled("Inject:  ", Style::default().fg(Color::Gray)),
                                Span::styled(i, Style::default().fg(Color::Green)),
                            ]));
                        }
                    }
                    autonoetic_types::background::ScheduledAction::WebFetch { url, timeout_secs, .. } => {
                        let url = url.clone();
                        let timeout_secs = *timeout_secs;
                        lines.push(Line::from(vec![
                            Span::styled("Type:    ", Style::default().fg(Color::Gray)),
                            Span::styled("web_fetch", Style::default().fg(Color::White)),
                        ]));
                        lines.push(Line::from(vec![
                            Span::styled("URL:     ", Style::default().fg(Color::Gray)),
                            Span::styled(url, Style::default().fg(Color::Green)),
                        ]));
                        if let Some(t) = timeout_secs {
                            lines.push(Line::from(vec![
                                Span::styled("Timeout: ", Style::default().fg(Color::Gray)),
                                Span::styled(format!("{t}s"), Style::default().fg(Color::Green)),
                            ]));
                        }
                    }
                    autonoetic_types::background::ScheduledAction::WebCall {
                        url, method, headers, body, ..
                    } => {
                        let url = url.clone();
                        let method = method.clone();
                        let headers = headers.clone();
                        let body = body.clone();
                        lines.push(Line::from(vec![
                            Span::styled("Type:    ", Style::default().fg(Color::Gray)),
                            Span::styled("web_call", Style::default().fg(Color::White)),
                        ]));
                        lines.push(Line::from(vec![
                            Span::styled("URL:     ", Style::default().fg(Color::Gray)),
                            Span::styled(url, Style::default().fg(Color::Green)),
                        ]));
                        if let Some(m) = method {
                            lines.push(Line::from(vec![
                                Span::styled("Method:  ", Style::default().fg(Color::Gray)),
                                Span::styled(m, Style::default().fg(Color::Green)),
                            ]));
                        }
                        if let Some(h) = headers {
                            if !h.is_empty() {
                                lines.push(Line::from(vec![
                                    Span::styled("Headers: ", Style::default().fg(Color::Gray)),
                                    Span::styled(
                                        serde_json::to_string(&h).unwrap_or_default(),
                                        Style::default().fg(Color::Green),
                                    ),
                                ]));
                            }
                        }
                        if let Some(b) = body {
                            lines.push(Line::from(vec![
                                Span::styled("Body:    ", Style::default().fg(Color::Gray)),
                                Span::styled(
                                    serde_json::to_string(&b).unwrap_or_default(),
                                    Style::default().fg(Color::Green),
                                ),
                            ]));
                        }
                    }
                    autonoetic_types::background::ScheduledAction::WebSearch { query, provider, .. } => {
                        let query = query.clone();
                        let provider = provider.clone();
                        lines.push(Line::from(vec![
                            Span::styled("Type:    ", Style::default().fg(Color::Gray)),
                            Span::styled("web_search", Style::default().fg(Color::White)),
                        ]));
                        lines.push(Line::from(vec![
                            Span::styled("Query:   ", Style::default().fg(Color::Gray)),
                            Span::styled(query, Style::default().fg(Color::Green)),
                        ]));
                        if let Some(p) = provider {
                            lines.push(Line::from(vec![
                                Span::styled("Provider:", Style::default().fg(Color::Gray)),
                                Span::styled(p, Style::default().fg(Color::Green)),
                            ]));
                        }
                    }
                    other => {
                        lines.push(Line::from(vec![
                            Span::styled("Action:  ", Style::default().fg(Color::Gray)),
                            Span::styled(other.kind(), Style::default().fg(Color::White)),
                        ]));
                    }
                }

                // Code excerpts for operator inspection.
                if let Some(ref excerpts) = items[idx].code_excerpts {
                    if !excerpts.is_empty() {
                        lines.push(Line::from(""));
                        lines.push(Line::from(Span::styled(
                            format!("Code Excerpts ({} file(s)) — press C to toggle", excerpts.len()),
                            Style::default().fg(Color::Magenta).add_modifier(Modifier::BOLD),
                        )));
                        for exc in excerpts {
                            lines.push(Line::from(Span::styled(
                                format!("  {} ({} bytes{})",
                                    exc.file_name,
                                    exc.size_bytes,
                                    if exc.truncated {
                                        format!(", truncated from {}", exc.truncated_from_bytes.unwrap_or(0))
                                    } else { String::new() },
                                ),
                                Style::default().fg(Color::Yellow),
                            )));
                            // Show code inline if small; otherwise note the C keybinding.
                            if exc.content.len() <= 500 {
                                for ln in exc.content.lines() {
                                    lines.push(Line::from(Span::raw(format!("    {}", ln))));
                                }
                            }
                        }
                    }
                }
                if let Some(ref risk) = items[idx].risk_summary {
                    lines.push(Line::from(""));
                    lines.push(Line::from(Span::styled(
                        "Risk Summary:",
                        Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                    )));
                    if risk.host_count > 0 {
                        lines.push(Line::from(format!("  Hosts: {}", risk.host_count)));
                    }
                    if !risk.dangerous_patterns.is_empty() {
                        for p in &risk.dangerous_patterns {
                            lines.push(Line::from(Span::styled(
                                format!("  ⚠ {}", p),
                                Style::default().fg(Color::Red),
                            )));
                        }
                    }
                    if let Some(ref v) = risk.auditor_verdict {
                        lines.push(Line::from(format!("  Auditor: {}", v)));
                    }
                }

                let selected_id = items[idx].request_id.clone();
                if last_selected != Some(idx) {
                    if let Ok(msgs) = gateway_store.get_gate_messages(&selected_id) {
                        enrichment_cache.insert(selected_id.clone(), msgs);
                    }
                    last_selected = Some(idx);
                }
                if let Some(msgs) = enrichment_cache.get(&selected_id) {
                    if !msgs.is_empty() {
                        lines.push(Line::from(""));
                        lines.push(Line::from(Span::styled(
                            "Enrichment:",
                            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
                        )));
                        for msg in msgs {
                            let sender = format!("[{}] ", msg.sender);
                            for (i, ln) in msg.content.lines().enumerate() {
                                if i == 0 {
                                    lines.push(Line::from(vec![
                                        Span::styled(sender.clone(), Style::default().fg(Color::DarkGray)),
                                        Span::styled(ln.to_string(), Style::default().fg(Color::Cyan)),
                                    ]));
                                } else {
                                    lines.push(Line::from(vec![
                                        Span::styled(
                                            format!("{:width$}", "", width = sender.len()),
                                            Style::default().fg(Color::DarkGray),
                                        ),
                                        Span::styled(ln.to_string(), Style::default().fg(Color::Cyan)),
                                    ]));
                                }
                            }
                        }
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

            // Fullscreen code view overlay (toggled with C).
            if show_code_fullscreen {
                if let Some(idx) = state.selected() {
                    if let Some(ref excerpts) = items[idx].code_excerpts {
                        let mut code_lines: Vec<Line> = Vec::new();
                        code_lines.push(Line::from(Span::styled(
                            "Code View (press C to close)",
                            Style::default().fg(Color::Magenta).add_modifier(Modifier::BOLD),
                        )));
                        code_lines.push(Line::from(""));
                        for exc in excerpts {
                            code_lines.push(Line::from(Span::styled(
                                format!("── {} ({} bytes{}) ──",
                                    exc.file_name,
                                    exc.size_bytes,
                                    if exc.truncated {
                                        format!(", truncated from {}", exc.truncated_from_bytes.unwrap_or(0))
                                    } else { String::new() },
                                ),
                                Style::default().fg(Color::Yellow),
                            )));
                            for ln in exc.content.lines() {
                                code_lines.push(Line::from(Span::raw(format!("  {}", ln))));
                            }
                            code_lines.push(Line::from(""));
                        }
                        let code_widget = Paragraph::new(code_lines)
                            .block(Block::default()
                                .borders(Borders::ALL)
                                .border_style(Style::default().fg(Color::Magenta))
                                .title(" Code "))
                            .wrap(Wrap { trim: false });
                        f.render_widget(code_widget, f.area());
                    }
                }
            }

            let status = if tui_mode == TuiMode::AskQuestion {
                // Show the question input prompt in the status bar
                let cursor_input = format!("{}_", question_input);
                Line::from(vec![
                    Span::styled(" ? Ask: ", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
                    Span::styled(cursor_input, Style::default().fg(Color::White)),
                    Span::styled("  [Enter: submit  Esc: cancel]", Style::default().fg(Color::DarkGray)),
                ])
            } else if tui_mode == TuiMode::WriteMessage {
                let cursor_input = format!("{}_", message_input);
                Line::from(vec![
                    Span::styled(" \u{270D} Note: ", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
                    Span::styled(cursor_input, Style::default().fg(Color::White)),
                    Span::styled("  [Enter: post  Esc: cancel]", Style::default().fg(Color::DarkGray)),
                ])
            } else if tui_mode == TuiMode::AskAgent {
                let cursor_input = format!("{}_", ask_agent_input);
                Line::from(vec![
                    Span::styled(" \u{1F916} Ask agent: ", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
                    Span::styled(cursor_input, Style::default().fg(Color::White)),
                    Span::styled("  [Enter: spawn clarification  Esc: cancel]", Style::default().fg(Color::DarkGray)),
                ])
            } else if status_msg.is_empty() {
                Line::from(Span::styled(
                    " \u{2191}\u{2193}/j/k: navigate  a: approve  r: reject  R: refresh  ?: ask  m: note  A: ask-agent  c: code  q: quit",
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

                // Q&A mode intercepts all keys for text input
                if tui_mode == TuiMode::AskQuestion {
                    match key.code {
                        KeyCode::Esc => {
                            tui_mode = TuiMode::Navigate;
                            question_input.clear();
                            question_answer.clear();
                        }
                        KeyCode::Enter => {
                            let q = question_input.trim().to_string();
                            if !q.is_empty() {
                                if let Some(idx) = state.selected() {
                                    question_answer =
                                        ask_approval_question_llm(&items[idx], &q, config)
                                            .await
                                            .unwrap_or_else(|e| format!("\u{26a0} {e}"));
                                }
                            }
                        }
                        KeyCode::Backspace => {
                            question_input.pop();
                            question_answer.clear();
                        }
                        KeyCode::Char(c) => {
                            question_input.push(c);
                            question_answer.clear();
                        }
                        _ => {}
                    }
                    continue;
                }

                // Ask-agent mode intercepts all keys for text input
                if tui_mode == TuiMode::AskAgent {
                    match key.code {
                        KeyCode::Esc => {
                            tui_mode = TuiMode::Navigate;
                            ask_agent_input.clear();
                            ask_agent_status.clear();
                        }
                        KeyCode::Enter => {
                            let q = ask_agent_input.trim().to_string();
                            if !q.is_empty() {
                                if let Some(idx) = state.selected() {
                                    let selected_id = items[idx].request_id.clone();
                                    let store_arc = std::sync::Arc::new(
                                        autonoetic_gateway::scheduler::gateway_store::GatewayStore::open(
                                            &autonoetic_gateway::execution::gateway_root_dir(config),
                                        ).map_err(|e| anyhow::anyhow!("{}", e))?,
                                    );
                                    let service = autonoetic_gateway::execution::GatewayExecutionService::new(
                                        config.clone(),
                                        Some(store_arc),
                                    );
                                    ask_agent_status = match service
                                        .spawn_clarification_for_approval(&selected_id, &q)
                                        .await
                                    {
                                        Ok(outcome) => {
                                            if let Ok(msgs) =
                                                gateway_store.get_gate_messages(&selected_id)
                                            {
                                                enrichment_cache.insert(selected_id, msgs);
                                            }
                                            outcome.answer
                                        }
                                        Err(e) => format!("\u{26a0} ask-agent failed: {e}"),
                                    };
                                }
                            }
                        }
                        KeyCode::Backspace => {
                            ask_agent_input.pop();
                            ask_agent_status.clear();
                        }
                        KeyCode::Char(c) => {
                            ask_agent_input.push(c);
                            ask_agent_status.clear();
                        }
                        _ => {}
                    }
                    continue;
                }

                // Message-write mode intercepts all keys for text input
                if tui_mode == TuiMode::WriteMessage {
                    match key.code {
                        KeyCode::Esc => {
                            tui_mode = TuiMode::Navigate;
                            message_input.clear();
                        }
                        KeyCode::Enter => {
                            let msg = message_input.trim().to_string();
                            if !msg.is_empty() {
                                if let Some(idx) = state.selected() {
                                    let selected_id = items[idx].request_id.clone();
                                    let redacted =
                                        autonoetic_gateway::log_redaction::redact_text_for_logs(
                                            &msg,
                                        );
                                    match gateway_store.add_gate_message(
                                        &selected_id,
                                        "operator",
                                        &redacted,
                                    ) {
                                        Ok(_) => {
                                            status_msg =
                                                format!("\u{1F4AC} Note posted to {}", selected_id);
                                            if let Ok(msgs) =
                                                gateway_store.get_gate_messages(&selected_id)
                                            {
                                                enrichment_cache.insert(selected_id, msgs);
                                            }
                                        }
                                        Err(e) => {
                                            status_msg = format!("\u{274C} Failed: {}", e);
                                        }
                                    }
                                }
                            }
                            tui_mode = TuiMode::Navigate;
                            message_input.clear();
                        }
                        KeyCode::Backspace => {
                            message_input.pop();
                        }
                        KeyCode::Char(c) => {
                            message_input.push(c);
                        }
                        _ => {}
                    }
                    continue;
                }

                match key.code {
                    KeyCode::Char('q') | KeyCode::Esc => {
                        done = true;
                    }
                    KeyCode::Char('?') => {
                        if state.selected().is_some() {
                            tui_mode = TuiMode::AskQuestion;
                            question_input.clear();
                            question_answer.clear();
                            status_msg.clear();
                        }
                    }
                    KeyCode::Char('m') => {
                        if state.selected().is_some() {
                            tui_mode = TuiMode::WriteMessage;
                            message_input.clear();
                            status_msg.clear();
                        }
                    }
                    KeyCode::Char('A') => {
                        if state.selected().is_some() {
                            tui_mode = TuiMode::AskAgent;
                            ask_agent_input.clear();
                            ask_agent_status.clear();
                            status_msg.clear();
                        }
                    }
                    KeyCode::Char('c') => {
                        show_code_fullscreen = !show_code_fullscreen;
                        if show_code_fullscreen {
                            status_msg = "Code fullscreen — press c to close".to_string();
                        } else {
                            status_msg.clear();
                        }
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

                            // RevisionPromote (R++2) requires per-capability
                            // acknowledgement that the TUI cannot collect
                            // safely. Direct the operator to the CLI form
                            // rather than letting `approve_request` always
                            // fail with a structured error.
                            if matches!(
                                req.action,
                                autonoetic_types::background::ScheduledAction::RevisionPromote { .. }
                            ) {
                                status_msg = format!(
                                    "{} approve from CLI: `gateway approvals approve {} --acknowledge-capability <TYPE>` for each added/broadened cap (R++2)",
                                    "\u{26a0}",
                                    req.request_id,
                                );
                                continue;
                            }

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
                                None,
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
                                enrichment_cache.clear();
                                last_selected = None;
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

/// Ask a natural-language question about an approval request using the configured LLM.
///
/// Serialises the full [`ApprovalRequest`] as JSON context and sends it to the
/// LLM together with the operator's question.  The LLM preset is resolved from
/// the gateway config (preference order: "fast" → "default" → first fixed preset).
async fn ask_approval_question_llm(
    req: &autonoetic_types::background::ApprovalRequest,
    question: &str,
    config: &autonoetic_types::config::GatewayConfig,
) -> anyhow::Result<String> {
    use autonoetic_gateway::llm::{build_driver, CompletionRequest, Message};
    use autonoetic_gateway::runtime::llm_preset_resolver::resolve_fixed_preset;

    // Resolve a fixed LLM preset (preference: "fast" → "default" → first available).
    let llm_config = ["fast", "default"]
        .iter()
        .find_map(|name| {
            config
                .llm_presets
                .get(*name)
                .and_then(|p| resolve_fixed_preset(p))
        })
        .or_else(|| {
            config
                .llm_presets
                .values()
                .find_map(|p| resolve_fixed_preset(p))
        })
        .ok_or_else(|| {
            anyhow::anyhow!(
                "No LLM preset configured. Add a 'fast' or 'default' preset to \
                 config.yaml to enable approval Q&A."
            )
        })?;

    let model = llm_config.model.clone();
    let client = reqwest::Client::new();
    let driver = build_driver(llm_config, client)?;

    let approval_json = serde_json::to_string_pretty(req)?;
    let request = CompletionRequest::simple(
        model,
        vec![
            Message::system(
                "You are a security assistant helping an operator review pending agent \
                 approval requests before deciding whether to approve or reject them. \
                 Answer questions concisely and factually based only on the provided \
                 approval data. Do not invent or infer information that is not present.",
            ),
            Message::user(format!(
                "Approval request data:\n```json\n{}\n```\n\nQuestion: {}",
                approval_json, question
            )),
        ],
    );

    let response = driver.complete(&request).await?;
    if response.text.is_empty() {
        anyhow::bail!("LLM returned an empty response");
    }
    Ok(response.text)
}

pub async fn handle_gateway_interactions(
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
            use autonoetic_types::background::{UserInteractionAnswer, UserInteractionStatus};

            if text.is_none() && option.is_none() {
                anyhow::bail!("Must provide either --text or --option to answer an interaction");
            }
            if text.is_some() && option.is_some() {
                anyhow::bail!("Provide exactly one of --text or --option");
            }

            let store = std::sync::Arc::new(gateway_store);

            // Validate the interaction exists and is pending
            let interaction = store
                .get_user_interaction(interaction_id)?
                .ok_or_else(|| anyhow::anyhow!("Unknown interaction '{}'", interaction_id))?;
            anyhow::ensure!(
                interaction.status == UserInteractionStatus::Pending,
                "Interaction '{}' is {:?}; only pending interactions can be answered",
                interaction_id,
                interaction.status
            );

            let answer = UserInteractionAnswer {
                interaction_id: interaction_id.clone(),
                answer_text: text.clone(),
                answer_option_id: option.clone(),
                answered_by: "cli".to_string(),
            };

            store.answer_user_interaction(&answer)?;

            println!("Answered interaction {}", interaction_id);
            if let Some(opt) = option {
                println!("  Selected option: {}", opt);
            }
            if let Some(txt) = text {
                println!("  Answer text: {}", txt);
            }

            // Workflow-bound: update task status to Runnable so the durable queue picks it up
            if let (Some(wf_id), Some(t_id)) = (
                interaction.workflow_id.as_deref(),
                interaction.task_id.as_deref(),
            ) {
                use autonoetic_types::workflow::TaskRunStatus;
                if let Some(task) =
                    autonoetic_gateway::scheduler::workflow_store::load_task_run(&config, Some(store.as_ref()), wf_id, t_id)?
                {
                    if task.status == TaskRunStatus::Paused {
                        autonoetic_gateway::scheduler::workflow_store::update_task_run_status(
                            &config,
                            Some(store.as_ref()),
                            wf_id,
                            t_id,
                            TaskRunStatus::Runnable,
                            Some("user interaction answered; task queued for execution".to_string()),
                            None,
                            None,
                        )?;
                        println!("  Workflow task {} queued for execution", t_id);
                    }
                }
            }

            println!();
            if interaction.workflow_id.is_some() {
                println!("The gateway daemon will resume the session automatically on its next tick.");
            } else {
                println!("Restart the session to apply the answer. For example:");
                println!("  autonoetic gateway start   (daemon will resume on next tick)");
                println!("  autonoetic chat --session {}   (interactive resume)", interaction.session_id);
            }
            if !interaction.workflow_id.is_some() {
                println!("If the gateway is not running, start it with: autonoetic gateway start");
            }
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

pub async fn handle_gateway_system_agents(
    config_path: &Path,
    command: &super::common::SystemAgentCommands,
) -> anyhow::Result<()> {
    let config = autonoetic_gateway::config::load_config(config_path)?;
    let gateway_dir = std::path::PathBuf::from(&config.agents_dir).join(".gateway");
    let store = std::sync::Arc::new(
        autonoetic_gateway::scheduler::gateway_store::GatewayStore::open(&gateway_dir)?,
    );

    match command {
        super::common::SystemAgentCommands::List { ref json } => {
            if config.system_agents.is_empty() {
                if *json {
                    println!("[]");
                } else {
                    println!("No system agents declared in config.");
                }
                return Ok(());
            }

            let scheduled = store.list_scheduled_jobs_for_owner("system", None, None)
                .unwrap_or_default();

            if *json {
                let entries: Vec<serde_json::Value> = config.system_agents.iter().map(|e| {
                    let active_job = scheduled.iter().find(|j| j.target_agent_id == e.agent_id && j.status == autonoetic_types::scheduled_job::ScheduledJobStatus::Active);
                    serde_json::json!({
                        "agent_id": e.agent_id,
                        "schedule": e.schedule,
                        "enabled": e.enabled,
                        "has_active_job": active_job.is_some(),
                        "job_id": active_job.map(|j| j.job_id.clone()),
                        "next_run_at": active_job.map(|j| j.next_run_at.clone()),
                    })
                }).collect();
                println!("{}", serde_json::to_string_pretty(&entries)?);
            } else {
                println!("{:<40} {:<20} {:<8} {:<10} {}", "AGENT_ID", "SCHEDULE", "ENABLED", "JOB", "NEXT_RUN");
                for e in &config.system_agents {
                    let active_job = scheduled.iter().find(|j| j.target_agent_id == e.agent_id && j.status == autonoetic_types::scheduled_job::ScheduledJobStatus::Active);
                    let schedule = e.schedule.as_deref().unwrap_or("(oneshot)");
                    let job_status = active_job.map(|j| j.job_id.as_str()).unwrap_or("-");
                    let next_run = active_job.map(|j| j.next_run_at.as_str()).unwrap_or("-");
                    println!("{:<40} {:<20} {:<8} {:<10} {}", e.agent_id, schedule, e.enabled, job_status, next_run);
                }
            }
        }

        super::common::SystemAgentCommands::Bootstrap => {
            let results = autonoetic_gateway::scheduler::system_agents::reconcile_system_agents(
                &config, &store,
            );
            if results.is_empty() {
                println!("No system agents to reconcile.");
            } else {
                for r in &results {
                    let icon = match r.action {
                        autonoetic_gateway::scheduler::system_agents::ReconcileAction::Created => "+",
                        autonoetic_gateway::scheduler::system_agents::ReconcileAction::SkippedExists => "=",
                        autonoetic_gateway::scheduler::system_agents::ReconcileAction::SkippedDisabled => "-",
                        autonoetic_gateway::scheduler::system_agents::ReconcileAction::SkippedMissing => "!",
                        autonoetic_gateway::scheduler::system_agents::ReconcileAction::SkippedNoSchedule => "o",
                        autonoetic_gateway::scheduler::system_agents::ReconcileAction::Failed => "x",
                    };
                    println!("{} {} {}", icon, r.agent_id, r.message);
                }
            }
        }

        super::common::SystemAgentCommands::Run { ref agent_id } => {
            let entry = config.system_agents.iter().find(|e| e.agent_id == *agent_id);
            if entry.is_none() {
                anyhow::bail!("Agent '{}' is not declared as a system agent in config.", agent_id);
            }

            let repo = autonoetic_gateway::agent::repository::AgentRepository::from_config(&config);
            let _loaded = repo.get_sync(&agent_id)
                .map_err(|e| anyhow::anyhow!("Could not load agent '{}': {}", agent_id, e))?;

            let message = entry.and_then(|e| e.message.clone())
                .unwrap_or_else(|| format!("Manual trigger for {}", agent_id));

            println!("Spawning system agent '{}'...", agent_id);

            let agent_ref = autonoetic_gateway::resolve_target_to_agent_ref(
                &agent_id, store.as_ref(),
            ).map_err(|e| anyhow::anyhow!("Could not resolve agent '{}': {}", agent_id, e))?;

            println!("Target: {} @ {}", agent_ref.agent_id, agent_ref.revision_id);
            println!("Message: {}", message);
            println!("Note: Actual agent execution requires a running gateway. Use 'autonoetic gateway start' to run the agent.");
        }
    }

    Ok(())
}

pub async fn handle_gateway_cron(
    config_path: &Path,
    command: &super::common::GatewayCronCommands,
) -> anyhow::Result<()> {
    let config = autonoetic_gateway::config::load_config(config_path)?;
    let gateway_dir = autonoetic_gateway::execution::gateway_root_dir(&config);
    let store =
        autonoetic_gateway::scheduler::gateway_store::GatewayStore::open(&gateway_dir)?;

    match command {
        super::common::GatewayCronCommands::List {
            status,
            owner,
            root_session,
            limit,
            json,
        } => {
            let status_filter = match status.as_deref() {
                None => None,
                Some("active") => {
                    Some(autonoetic_types::scheduled_job::ScheduledJobStatus::Active)
                }
                Some("paused") => {
                    Some(autonoetic_types::scheduled_job::ScheduledJobStatus::Paused)
                }
                Some("cancelled") => {
                    Some(autonoetic_types::scheduled_job::ScheduledJobStatus::Cancelled)
                }
                Some(other) => {
                    anyhow::bail!(
                        "invalid --status '{}': expected active | paused | cancelled",
                        other
                    );
                }
            };
            let jobs = store.list_scheduled_jobs(
                owner.as_deref(),
                root_session.as_deref(),
                status_filter,
                *limit,
            )?;

            if *json {
                println!("{}", serde_json::to_string_pretty(&jobs)?);
                return Ok(());
            }

            if jobs.is_empty() {
                println!("No scheduled cron jobs match the filters.");
                return Ok(());
            }

            println!(
                "{:<14} {:<22} {:<16} {:<10} {:<18} {:<38} {}",
                "JOB_ID", "TARGET", "CRON", "STATUS", "OWNER", "ROOT_SESSION", "NEXT_RUN"
            );
            for job in &jobs {
                let target = format!("{}@{}", job.target_agent_id, job.target_revision_id);
                let target = truncate_field(&target, 22);
                let cron = truncate_field(&job.cron_expr, 16);
                let owner = truncate_field(&job.owner_agent_id, 18);
                let root = truncate_field(&job.root_session_id, 38);
                let next = truncate_field(&job.next_run_at, 24);
                println!(
                    "{:<14} {:<22} {:<16} {:<10} {:<18} {:<38} {}",
                    truncate_field(&job.job_id, 14),
                    target,
                    cron,
                    job.status,
                    owner,
                    root,
                    next,
                );
                if let Some(err) = job.last_error.as_deref().filter(|s| !s.is_empty()) {
                    println!("  last_error: {}", truncate_field(err, 120));
                }
            }
            println!();
            println!(
                "Reconnect to a session timeline: autonoetic room <root_session_id> --tui"
            );
        }
    }

    Ok(())
}

fn truncate_field(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else if max <= 3 {
        s.chars().take(max).collect()
    } else {
        format!("{}...", &s[..max.saturating_sub(3)])
    }
}

// ---------------------------------------------------------------------------
// Constitution amendment proposals — R+++1 (issue #92)
// ---------------------------------------------------------------------------

const PROPOSAL_DECISION_STATES: &[&str] = &["approved", "rejected", "deferred"];

pub async fn handle_gateway_constitution(
    config_path: &Path,
    command: &super::common::GatewayConstitutionCommands,
) -> anyhow::Result<()> {
    let config = autonoetic_gateway::config::load_config(config_path)?;
    // The SQLite store is opened lazily — only the proposal subcommands need
    // it. `show` is read-only (config + signed constitution text) and must not
    // touch the DB or run migrations, so it works on a read-only/permissionless
    // filesystem.
    let open_store = || -> anyhow::Result<_> {
        let gateway_dir = std::path::PathBuf::from(&config.agents_dir).join(".gateway");
        autonoetic_gateway::scheduler::gateway_store::GatewayStore::open(&gateway_dir)
    };

    match command {
        super::common::GatewayConstitutionCommands::Show {
            include_text,
            json,
        } => {
            autonoetic_gateway::constitution_digest::initialize_constitution(&config)?;
            let profile =
                autonoetic_gateway::constitution_digest::constitution_profile(*include_text);
            if *json {
                println!("{}", serde_json::to_string_pretty(&profile)?);
                return Ok(());
            }
            println!("Constitution {}", profile.version);
            println!("  Digest:  {}", profile.digest);
            println!("  Format:  v{}", profile.format_version);
            match (&profile.signer_id, profile.signed) {
                (Some(signer), true) => println!("  Signed:  yes (signer {signer})"),
                _ => println!("  Signed:  no"),
            }
            println!(
                "  Enforced: {} rules (P-*), {} rights (Ri-*)",
                profile.rule_enforcement_count, profile.right_enforcement_count
            );
            println!("\nClauses ({}):", profile.clauses.len());
            for c in &profile.clauses {
                let mark = if c.enforcement.is_some() { "✓" } else { " " };
                println!("  {mark} {:<8} [{}] {}", c.id, c.binds, c.gloss);
            }
            if let Some(text) = &profile.text {
                println!("\n--- constitution.md ---\n{text}");
            }
            Ok(())
        }
        super::common::GatewayConstitutionCommands::Proposals { command } => {
            handle_constitution_proposals(&open_store()?, command)
        }
        super::common::GatewayConstitutionCommands::Release { tag, json } => {
            let ids = open_store()?.publish_approved_proposals(tag)?;
            if *json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "release_tag": tag,
                        "published_proposal_ids": ids,
                        "count": ids.len(),
                    }))?
                );
            } else if ids.is_empty() {
                println!("No approved proposals to publish.");
            } else {
                println!("Marked {} proposal(s) with release tag '{}':", ids.len(), tag);
                for id in &ids {
                    println!("  {}", id);
                }
                println!(
                    "\nNote: edit {} to apply the proposal text. \
                     The constitution digest will bump on the next gateway rebuild.",
                    config.constitution.source_path.display()
                );
            }
            Ok(())
        }
    }
}

fn handle_constitution_proposals(
    store: &autonoetic_gateway::scheduler::gateway_store::GatewayStore,
    command: &super::common::GatewayConstitutionProposalCommands,
) -> anyhow::Result<()> {
    use super::common::GatewayConstitutionProposalCommands;

    match command {
        GatewayConstitutionProposalCommands::List {
            status,
            proposer,
            limit,
            json,
        } => {
            let rows = store.list_constitutional_proposals(
                status.as_deref(),
                proposer.as_deref(),
                *limit,
            )?;
            if *json {
                println!("{}", serde_json::to_string_pretty(&rows)?);
                return Ok(());
            }
            if rows.is_empty() {
                println!("No constitutional proposals found.");
                return Ok(());
            }
            println!(
                "{:<24} {:<14} {:<14} {:<24} {}",
                "PROPOSAL_ID", "STATUS", "KIND", "PROPOSER", "TARGET"
            );
            for p in &rows {
                println!(
                    "{:<24} {:<14} {:<14} {:<24} {}",
                    p.proposal_id,
                    p.status,
                    p.kind,
                    p.proposer_agent_id,
                    p.target_id.as_deref().unwrap_or("-"),
                );
            }
            Ok(())
        }
        GatewayConstitutionProposalCommands::Show { proposal_id, json } => {
            let proposal = store.get_constitutional_proposal(proposal_id)?;
            match proposal {
                None => {
                    anyhow::bail!("No proposal with id '{}'", proposal_id);
                }
                Some(p) => {
                    if *json {
                        println!("{}", serde_json::to_string_pretty(&p)?);
                    } else {
                        println!("Proposal: {}", p.proposal_id);
                        println!("  Status:        {}", p.status);
                        println!("  Kind:          {}", p.kind);
                        println!("  Target ID:     {}", p.target_id.as_deref().unwrap_or("-"));
                        println!("  Proposer:      {}", p.proposer_agent_id);
                        if let Some(s) = &p.proposer_session_id {
                            println!("  From session:  {}", s);
                        }
                        println!("  Created at:    {}", p.created_at);
                        if let Some(d) = &p.decided_at {
                            println!("  Decided at:    {}", d);
                        }
                        if let Some(b) = &p.decided_by {
                            println!("  Decided by:    {}", b);
                        }
                        if let Some(reason) = &p.decision_reason {
                            println!("  Reason:        {}", reason);
                        }
                        if let Some(rel) = &p.published_in_release {
                            println!("  Released in:   {}", rel);
                        }
                        println!("  Justification: {}", p.justification);
                        if let Some(t) = &p.proposed_text {
                            println!("\n--- proposed text ---\n{}", t);
                        }
                        if !matches!(p.evidence_json, serde_json::Value::Null) {
                            println!(
                                "\n--- evidence ---\n{}",
                                serde_json::to_string_pretty(&p.evidence_json)?
                            );
                        }
                    }
                    Ok(())
                }
            }
        }
        GatewayConstitutionProposalCommands::Approve {
            proposal_id,
            reason,
        } => decide_proposal(store, proposal_id, "approved", reason.as_deref()),
        GatewayConstitutionProposalCommands::Reject {
            proposal_id,
            reason,
        } => decide_proposal(store, proposal_id, "rejected", reason.as_deref()),
        GatewayConstitutionProposalCommands::Defer {
            proposal_id,
            reason,
        } => decide_proposal(store, proposal_id, "deferred", reason.as_deref()),
    }
}

fn decide_proposal(
    store: &autonoetic_gateway::scheduler::gateway_store::GatewayStore,
    proposal_id: &str,
    new_status: &str,
    reason: Option<&str>,
) -> anyhow::Result<()> {
    debug_assert!(PROPOSAL_DECISION_STATES.contains(&new_status));
    let updated = store.decide_constitutional_proposal(proposal_id, new_status, "operator", reason)?;
    if !updated {
        anyhow::bail!("No proposal with id '{}'", proposal_id);
    }
    println!("Proposal {} → {}", proposal_id, new_status);
    if let Some(r) = reason {
        println!("  Reason: {}", r);
    }
    Ok(())
}

pub async fn handle_gateway_wiki(
    config_path: &Path,
    command: &super::common::GatewayWikiCommands,
) -> anyhow::Result<()> {
    let config = autonoetic_gateway::config::load_config(config_path)?;
    let gateway_dir = autonoetic_gateway::execution::gateway_root_dir(&config);
    let gateway_store =
        autonoetic_gateway::scheduler::gateway_store::GatewayStore::open(&gateway_dir)?;

    match command {
        super::common::GatewayWikiCommands::Proposals { json } => {
            let approvals = autonoetic_gateway::scheduler::load_approval_requests(
                &config,
                Some(&gateway_store),
            )?;
            let wiki: Vec<_> = approvals
                .into_iter()
                .filter(|a| {
                    matches!(
                        a.action,
                        autonoetic_types::background::ScheduledAction::WikiProposal { .. }
                    )
                })
                .collect();
            if *json {
                println!("{}", serde_json::to_string_pretty(&wiki)?);
                return Ok(());
            }
            if wiki.is_empty() {
                println!("No pending wiki proposals.");
                return Ok(());
            }
            println!("{:<38} {:<20} {:<20} {}", "REQUEST ID", "AGENT", "TITLE", "PAGE ID");
            for a in &wiki {
                let (title, page_id) = match &a.action {
                    autonoetic_types::background::ScheduledAction::WikiProposal {
                        title, page_id, ..
                    } => (title.as_str(), page_id.as_str()),
                    _ => unreachable!(),
                };
                println!(
                    "{:<38} {:<20} {:<20} {}",
                    a.request_id, a.agent_id, title, page_id
                );
            }
        }
        super::common::GatewayWikiCommands::Promote { request_id, reason } => {
            let decision = autonoetic_gateway::scheduler::approve_request_with_options(
                &config,
                Some(&gateway_store),
                request_id,
                "cli",
                reason.clone(),
                None,
                None,
                None,
                autonoetic_gateway::scheduler::ApproveOptions::default(),
            )?;
            println!(
                "Wiki proposal promoted: {} — {}",
                decision.request_id,
                if let autonoetic_types::background::ScheduledAction::WikiProposal { title, .. } =
                    &decision.action
                {
                    title.as_str()
                } else {
                    "(unknown)"
                }
            );
        }
        super::common::GatewayWikiCommands::Reject { request_id, reason } => {
            let decision = autonoetic_gateway::scheduler::reject_request(
                &config,
                Some(&gateway_store),
                request_id,
                "cli",
                reason.clone(),
                None,
            )?;
            println!(
                "Wiki proposal rejected: {} — {}",
                decision.request_id,
                if let autonoetic_types::background::ScheduledAction::WikiProposal { title, .. } =
                    &decision.action
                {
                    title.as_str()
                } else {
                    "(unknown)"
                }
            );
        }
    }
    Ok(())
}
