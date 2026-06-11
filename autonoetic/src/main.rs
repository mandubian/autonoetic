mod cli;

use clap::Parser;
use cli::common::{dirs_or_default, mcp_registry_path, Cli, Commands};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    let log_level = cli.log_level.as_deref().unwrap_or("info");
    let env_filter =
        tracing_subscriber::EnvFilter::try_new(format!("autonoetic={log_level},{log_level}"))
            .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("autonoetic=info,info"));

    let config_path = cli
        .config
        .map(|s| std::path::PathBuf::from(s))
        .unwrap_or_else(|| dirs_or_default().join("config.yaml"));

    // `tracing_subscriber::fmt` defaults to stdout; the chat TUI also uses stdout (ratatui), so
    // INFO lines (e.g. gateway events) corrupt the alternate screen. `chat` and `run` therefore
    // log only to a rolling file under {agents_dir}/.gateway/logs/. `gateway start` still mirrors
    // to stderr (daemon/long-running server). Other commands log to stderr.
    let is_gateway_start = matches!(
        &cli.command,
        Commands::Gateway(args) if matches!(args.command, cli::common::GatewayCommands::Start { .. })
    );
    let is_chat = matches!(&cli.command, Commands::Chat(_));
    let is_run = matches!(&cli.command, Commands::Run(_));

    if is_gateway_start {
        let config = autonoetic_gateway::config::load_config(&config_path)?;
        let log_dir = config.agents_dir.join(".gateway").join("logs");
        std::fs::create_dir_all(&log_dir)?;
        let file_appender = tracing_appender::rolling::RollingFileAppender::builder()
            .rotation(tracing_appender::rolling::Rotation::DAILY)
            .max_log_files(5)
            .filename_prefix("gateway")
            .filename_suffix("log")
            .build(&log_dir)
            .map_err(|e| anyhow::anyhow!("Failed to create log appender: {}", e))?;
        let (non_blocking, _guard) = tracing_appender::non_blocking(file_appender);

        tracing_subscriber::registry()
            .with(env_filter)
            .with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr))
            .with(
                tracing_subscriber::fmt::layer()
                    .with_writer(non_blocking)
                    .with_ansi(false),
            )
            .init();

        // Keep the guard alive for the process lifetime
        // (it gets dropped when main returns, which flushes remaining logs)
        std::mem::forget(_guard);
    } else if is_chat || is_run {
        // When the YAML is missing, `load_config` returns defaults whose `agents_dir` is `./agents`
        // (cwd-relative). `autonoetic run` then creates `{config_dir}/config.yaml` with
        // `agents_dir: {config_dir}/agents` — so logging would otherwise land in the wrong tree until
        // the file exists. Match that layout when the config file is absent.
        let log_dir = if config_path.exists() {
            let config = autonoetic_gateway::config::load_config(&config_path)?;
            config.agents_dir.join(".gateway").join("logs")
        } else {
            config_path
                .parent()
                .unwrap_or_else(|| std::path::Path::new("."))
                .join("agents")
                .join(".gateway")
                .join("logs")
        };
        std::fs::create_dir_all(&log_dir)?;
        let filename_prefix = if is_run {
            "run"
        } else {
            "chat-cli"
        };
        let file_appender = tracing_appender::rolling::RollingFileAppender::builder()
            .rotation(tracing_appender::rolling::Rotation::DAILY)
            .max_log_files(5)
            .filename_prefix(filename_prefix)
            .filename_suffix("log")
            .build(&log_dir)
            .map_err(|e| anyhow::anyhow!("Failed to create log appender: {}", e))?;
        let (non_blocking, _guard) = tracing_appender::non_blocking(file_appender);

        tracing_subscriber::registry()
            .with(env_filter)
            .with(
                tracing_subscriber::fmt::layer()
                    .with_writer(non_blocking)
                    .with_ansi(false),
            )
            .init();

        std::mem::forget(_guard);
    } else {
        let base_dir = dirs_or_default();
        std::fs::create_dir_all(&base_dir)?;

        tracing_subscriber::registry()
            .with(env_filter)
            .with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr))
            .init();
    }

    std::env::set_var(
        "AUTONOETIC_MCP_REGISTRY_PATH",
        mcp_registry_path(&config_path).display().to_string(),
    );

    match &cli.command {
        Commands::Run(args) => {
            cli::run::handle_run(Some(config_path.to_str().unwrap_or("config.yaml")), args).await?;
        }
        Commands::Gateway(args) => match &args.command {
            cli::common::GatewayCommands::Start {
                daemon,
                port,
                tls,
                response_validation,
            } => {
                cli::gateway::handle_gateway_start(
                    &config_path,
                    *daemon,
                    *port,
                    *tls,
                    *response_validation,
                )
                .await?;
            }
            cli::common::GatewayCommands::Stop => {
                cli::gateway::handle_gateway_stop();
            }
            cli::common::GatewayCommands::Preflight { json } => {
                cli::gateway::handle_gateway_preflight(*json)?;
            }
            cli::common::GatewayCommands::Status { json } => {
                cli::gateway::handle_gateway_status(&config_path, *json).await?;
            }
            cli::common::GatewayCommands::Approvals { command } => {
                cli::gateway::handle_gateway_approvals(&config_path, command).await?;
            }
            cli::common::GatewayCommands::Grants { command } => {
                cli::gateway::handle_gateway_grants(&config_path, command).await?;
            }
            cli::common::GatewayCommands::Interactions { command } => {
                cli::gateway::handle_gateway_interactions(&config_path, command).await?;
            }
            cli::common::GatewayCommands::SystemAgents { command } => {
                cli::gateway::handle_gateway_system_agents(&config_path, command).await?;
            }
            cli::common::GatewayCommands::Cron { command } => {
                cli::gateway::handle_gateway_cron(&config_path, command).await?;
            }
            cli::common::GatewayCommands::Constitution { command } => {
                cli::gateway::handle_gateway_constitution(&config_path, command).await?;
            }
            cli::common::GatewayCommands::Wiki { command } => {
                cli::gateway::handle_gateway_wiki(&config_path, command).await?;
            }
        },

        Commands::Agent(args) => match &args.command {
            cli::common::AgentCommands::Init {
                agent_id,
                template,
                preset,
                provider,
                model,
            } => {
                cli::agent::init_agent_scaffold(
                    &config_path,
                    agent_id,
                    template.as_deref(),
                    preset.as_deref(),
                    provider.as_deref(),
                    model.as_deref(),
                )?;
            }
            cli::common::AgentCommands::Presets => {
                cli::agent::handle_agent_presets(&config_path)?;
            }
            cli::common::AgentCommands::InitConfig { output, overwrite } => {
                cli::agent::handle_init_config(output.as_deref(), *overwrite)?;
            }
            cli::common::AgentCommands::Run {
                agent_id,
                message,
                interactive,
                headless,
                response_validation,
                record_network,
                recording_duration,
                recording_max_requests,
                recording_max_bytes,
            } => {
                cli::agent::handle_agent_run(
                    &config_path,
                    agent_id,
                    message.as_deref(),
                    *interactive,
                    *headless,
                    *response_validation,
                    *record_network,
                    *recording_duration,
                    *recording_max_requests,
                    *recording_max_bytes,
                )
                .await?;
            }
            cli::common::AgentCommands::List => {
                cli::agent::handle_agent_list(&config_path).await?;
            }
            cli::common::AgentCommands::Bootstrap { from, overwrite, refresh_models } => {
                cli::agent::handle_agent_bootstrap(&config_path, from.as_deref(), *overwrite)?;
                if *refresh_models {
                    cli::run::refresh_models(&config_path).await?;
                }
            }
            cli::common::AgentCommands::Alias { command } => {
                cli::agent::handle_agent_alias(&config_path, command)?;
            }
            cli::common::AgentCommands::Seed {
                agent_id,
                revision_id,
                promotion_id,
                reason,
                json,
            } => {
                cli::agent::handle_agent_seed(
                    &config_path,
                    agent_id,
                    revision_id,
                    promotion_id.as_deref(),
                    reason.as_deref(),
                    *json,
                )?;
            }
            cli::common::AgentCommands::Revision { command } => {
                cli::agent::handle_agent_revision(&config_path, command)?;
            }
            cli::common::AgentCommands::PromotionHistory { agent_id, json } => {
                cli::agent::handle_agent_promotion_history(
                    &config_path,
                    agent_id.as_deref(),
                    *json,
                )?;
            }
            cli::common::AgentCommands::ImportSkill {
                from,
                agent_id,
                trust,
                provider,
                model,
            } => {
                cli::agent::handle_agent_import_skill(
                    &config_path,
                    from,
                    agent_id,
                    *trust,
                    provider.as_deref(),
                    model.as_deref(),
                )?;
            }
            cli::common::AgentCommands::Credential { command } => {
                cli::agent::handle_agent_credential(&config_path, command)?;
            }
        },

        Commands::Chat(args) => {
            cli::chat::handle_chat(&config_path, &args).await?;
        }

        Commands::Trace(args) => match &args.command {
            cli::common::TraceCommands::Sessions { agent, json } => {
                cli::trace::handle_trace_sessions(&config_path, agent.as_deref(), *json)?;
            }
            cli::common::TraceCommands::Show {
                session_id,
                agent,
                json,
            } => {
                cli::trace::handle_trace_session(
                    &config_path,
                    session_id,
                    agent.as_deref(),
                    *json,
                )?;
            }
            cli::common::TraceCommands::Event {
                log_id,
                agent,
                json,
            } => {
                cli::trace::handle_trace_event(&config_path, log_id, agent.as_deref(), *json)?;
            }
            cli::common::TraceCommands::Rebuild {
                session_id,
                agent,
                json,
                skip_checks,
            } => {
                cli::trace::handle_trace_rebuild(
                    &config_path,
                    session_id,
                    agent.as_deref(),
                    *json,
                    *skip_checks,
                )?;
            }
            cli::common::TraceCommands::Follow {
                session_id,
                agent,
                json,
            } => {
                cli::trace::handle_trace_follow(&config_path, session_id, agent.as_deref(), *json)
                    .await?;
            }
            cli::common::TraceCommands::Fork {
                session_id,
                message,
                new_session_id,
                at_turn,
                agent,
                interactive,
                json,
            } => {
                cli::trace::handle_trace_fork(
                    &config_path,
                    session_id,
                    message.as_deref(),
                    new_session_id.as_deref(),
                    *at_turn,
                    agent.as_deref(),
                    *interactive,
                    *json,
                )
                .await?;
            }
            cli::common::TraceCommands::History {
                session_id,
                agent,
                json,
            } => {
                cli::trace::handle_trace_history(
                    &config_path,
                    session_id,
                    agent.as_deref(),
                    *json,
                )?;
            }
            cli::common::TraceCommands::Digest { session_id, json } => {
                cli::trace::handle_trace_digest(&config_path, session_id, *json)?;
            }
            cli::common::TraceCommands::Workflow {
                workflow_or_root,
                root,
                json,
                follow,
            } => {
                cli::trace::handle_trace_workflow(
                    &config_path,
                    workflow_or_root,
                    *root,
                    *json,
                    *follow,
                )
                .await?;
            }
            cli::common::TraceCommands::Graph {
                session_or_workflow,
                json,
                follow,
            } => {
                cli::trace::handle_trace_graph(&config_path, session_or_workflow, *json, *follow)
                    .await?;
            }
            cli::common::TraceCommands::ContractHealth { since, json } => {
                cli::trace::handle_trace_contract_health(
                    &config_path,
                    since.as_deref(),
                    *json,
                )?;
            }
        },

        Commands::Room(args) => {
            cli::room::handle_room(&config_path, args).await?;
        }

        Commands::Skill(args) => match &args.command {
            cli::common::SkillCommands::Install { url_or_id, agent } => {
                tracing::info!("Installing Skill {} (agent: {:?})", url_or_id, agent);
            }
            cli::common::SkillCommands::Uninstall { skill_name, agent } => {
                tracing::info!("Uninstalling Skill {} from agent {}", skill_name, agent);
            }
        },

        Commands::Federate(args) => match &args.command {
            cli::common::FederateCommands::Join { peer_address } => {
                tracing::info!("Joining peer {}", peer_address);
            }
            cli::common::FederateCommands::List => {
                tracing::info!("Listing peers");
            }
        },

        Commands::Mcp(args) => match &args.command {
            cli::common::McpCommands::Add {
                server_name,
                command,
                sse_url,
                args,
            } => {
                cli::mcp::handle_mcp_add(
                    &config_path,
                    server_name.clone(),
                    command.clone(),
                    sse_url.clone(),
                    args.clone(),
                )
                .await?;
            }
            cli::common::McpCommands::Expose { agent_id } => {
                cli::mcp::handle_mcp_expose(agent_id, &config_path).await?;
            }
        },

        Commands::Security(args) => match &args.command {
            cli::common::SecurityCommands::Status { json } => {
                cli::security::handle_security_status(&config_path, *json)?;
            }
            cli::common::SecurityCommands::Findings {
                severity,
                finding_type,
                triage,
                limit,
                json,
            } => {
                cli::security::handle_security_findings(
                    &config_path,
                    severity.as_deref(),
                    finding_type.as_deref(),
                    triage.as_deref(),
                    *limit,
                    *json,
                )?;
            }
            cli::common::SecurityCommands::Triage {
                finding_id,
                state,
                reason,
            } => {
                cli::security::handle_security_triage(
                    &config_path,
                    finding_id,
                    state,
                    reason.as_deref(),
                )?;
            }
            cli::common::SecurityCommands::BulkTriage {
                state,
                reason,
                severity,
                finding_type,
                dry_run,
            } => {
                cli::security::handle_security_triage_bulk(
                    &config_path,
                    state,
                    reason,
                    severity.as_deref(),
                    finding_type.as_deref(),
                    *dry_run,
                )?;
            }
            cli::common::SecurityCommands::Patterns { status, limit, json } => {
                cli::security::handle_security_patterns(
                    &config_path,
                    status.as_deref(),
                    *limit,
                    *json,
                )?;
            }
            cli::common::SecurityCommands::PatternAccept {
                pattern_id,
                check_type,
                notes,
            } => {
                cli::security::handle_security_pattern_accept(
                    &config_path,
                    pattern_id,
                    check_type,
                    notes.as_deref(),
                )?;
            }
            cli::common::SecurityCommands::PatternReject { pattern_id, notes } => {
                cli::security::handle_security_pattern_reject(
                    &config_path,
                    pattern_id,
                    notes.as_deref(),
                )?;
            }
        },
        Commands::Review(args) => match &args.command {
            cli::common::ReviewCommands::Status { agent, json } => {
                // Simple inline handler
                let config = autonoetic_gateway::config::load_config(&config_path)?;
                let gateway_dir = config.agents_dir.join(".gateway");
                let store = std::sync::Arc::new(
                    autonoetic_gateway::scheduler::gateway_store::GatewayStore::open(&gateway_dir)?,
                );
                if *json {
                    let reviews = if let Some(aid) = agent {
                        store.list_post_promotion_reviews(Some(aid), 1)?
                    } else {
                        store.list_post_promotion_reviews(None, 100)?
                    };
                    println!("{}", serde_json::to_string_pretty(&reviews)?);
                } else {
                    let reviews = if let Some(aid) = agent {
                        store.list_post_promotion_reviews(Some(aid), 1)?
                    } else {
                        store.list_post_promotion_reviews(None, 100)?
                    };
                    if reviews.is_empty() {
                        eprintln!("No post-promotion reviews found.");
                    } else {
                        println!("{:<40} {:<20} {:<20} {:<10} {:<20}", "Review ID", "Agent", "Revision", "Findings", "Reviewed At");
                        println!("{}", "-".repeat(110));
                        for r in &reviews {
                            let findings_count: usize =
                                serde_json::from_str::<Vec<serde_json::Value>>(&r.findings_json)
                                    .map(|v| v.len())
                                    .unwrap_or(0);
                            println!(
                                "{:<40} {:<20} {:<20} {:<10} {:<20}",
                                r.review_id,
                                r.agent_id,
                                &r.revision_id[..r.revision_id.len().min(20)],
                                findings_count,
                                &r.reviewed_at[..19],
                            );
                        }
                    }
                }
            }
            cli::common::ReviewCommands::Inspect { review_id, json } => {
                let config = autonoetic_gateway::config::load_config(&config_path)?;
                let gateway_dir = config.agents_dir.join(".gateway");
                let store = std::sync::Arc::new(
                    autonoetic_gateway::scheduler::gateway_store::GatewayStore::open(&gateway_dir)?,
                );
                // Load the last review for each agent and find the matching one
                let reviews = store.list_post_promotion_reviews(None, 1000)?;
                let review = reviews.into_iter().find(|r| r.review_id == *review_id)
                    .ok_or_else(|| anyhow::anyhow!("Review '{}' not found", review_id))?;
                if *json {
                    println!("{}", serde_json::to_string_pretty(&review)?);
                } else {
                    println!("Review: {}", review.review_id);
                    println!("  Agent:        {}", review.agent_id);
                    println!("  Revision:     {}", review.revision_id);
                    println!("  Reviewed At:  {}", review.reviewed_at);
                    println!("  Tool failures: {}", review.tool_failures);
                    println!("  Auth denials:  {}", review.auth_denials);
                    println!("  Suspensions:   {}", review.suspensions);
                    println!("  Sentinel:      {}", review.sentinel_findings);
                    if let Ok(findings) =
                        serde_json::from_str::<Vec<serde_json::Value>>(&review.findings_json)
                    {
                        for f in &findings {
                            println!("  - [{}] {}", f["severity"], f["message"]);
                        }
                    }
                }
            }
            cli::common::ReviewCommands::History {
                agent,
                limit,
                json,
            } => {
                let config = autonoetic_gateway::config::load_config(&config_path)?;
                let gateway_dir = config.agents_dir.join(".gateway");
                let store = std::sync::Arc::new(
                    autonoetic_gateway::scheduler::gateway_store::GatewayStore::open(&gateway_dir)?,
                );
                let reviews =
                    store.list_post_promotion_reviews(agent.as_deref(), *limit)?;
                if *json {
                    println!("{}", serde_json::to_string_pretty(&reviews)?);
                } else {
                    if reviews.is_empty() {
                        eprintln!("No review history found.");
                    } else {
                        println!("{:<40} {:<20} {:<10} {:<10} {:<10} {:<20}", "Review ID", "Agent", "Failures", "Denials", "Suspensions", "Reviewed At");
                        println!("{}", "-".repeat(110));
                        for r in &reviews {
                            println!(
                                "{:<40} {:<20} {:<10} {:<10} {:<10} {:<20}",
                                r.review_id,
                                r.agent_id,
                                r.tool_failures,
                                r.auth_denials,
                                r.suspensions,
                                &r.reviewed_at[..19],
                            );
                        }
                    }
                }
            }
        },
        Commands::Eval(args) => match &args.command {
            cli::common::EvalCommands::Sealed {
                artifact_ref,
                fixture_set,
                agent_id,
                json,
                timeout,
            } => {
                cli::eval::handle_eval_sealed(
                    &config_path,
                    artifact_ref,
                    fixture_set,
                    agent_id,
                    *json,
                    *timeout,
                )?;
            }
        },
        Commands::Recording(args) => match &args.command {
            cli::common::RecordingCommands::List { agent, limit, json } => {
                cli::recording::handle_recording_list(&config_path, agent.as_deref(), *limit, *json)?;
            }
            cli::common::RecordingCommands::Inspect { session_id, json } => {
                cli::recording::handle_recording_inspect(&config_path, session_id, *json)?;
            }
            cli::common::RecordingCommands::Delete { session_id } => {
                cli::recording::handle_recording_delete(&config_path, session_id)?;
            }
            cli::common::RecordingCommands::Cancel { session_id } => {
                cli::recording::handle_recording_cancel(&config_path, session_id)?;
            }
        },
        Commands::Watchdog(args) => {
            cli::watchdog::handle_watchdog(&config_path, &args.session_id).await?;
        }
        Commands::SentinelExperiment(args) => {
            cli::sentinel_experiment::handle_sentinel_experiment(&config_path, args).await?;
        }
        Commands::Session(args) => {
            cli::session::handle_session(&config_path, &args.command).await?;
        }
        Commands::Improve(args) => {
            cli::improve::handle_improve(&config_path, &args.command).await?;
        }
        Commands::Capsule(args) => match &args.command {
            cli::common::CapsuleCommands::Export {
                agent_id,
                mode,
                revision,
                include_memory,
                sign,
                output,
                session_id,
                root_session_id,
                json,
            } => {
                cli::capsule::handle_export(
                    &config_path,
                    agent_id,
                    mode,
                    revision.as_deref(),
                    *include_memory,
                    *sign,
                    output.as_deref(),
                    session_id.as_deref(),
                    root_session_id.as_deref(),
                    *json,
                )?;
            }
            cli::common::CapsuleCommands::Import {
                archive,
                verify_signature,
                activate,
                dry_run,
                trust_domain,
                memory_conflict,
                json,
            } => {
                cli::capsule::handle_import(
                    &config_path,
                    archive,
                    *verify_signature,
                    *activate,
                    *dry_run,
                    trust_domain.as_deref(),
                    memory_conflict,
                    *json,
                )?;
            }
            cli::common::CapsuleCommands::Verify { archive, json } => {
                cli::capsule::handle_verify(&config_path, archive, *json)?;
            }
            cli::common::CapsuleCommands::Inspect { archive, json } => {
                cli::capsule::handle_inspect(&config_path, archive, *json)?;
            }
        },
    }

    Ok(())
}
