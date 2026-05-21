use std::path::Path;
use std::sync::Arc;

use anyhow::Context;
use autonoetic_gateway::runtime::tools::improvement::AbReplayTool;
use autonoetic_gateway::runtime::tools::NativeTool;
use autonoetic_types::agent::{AgentIdentity, AgentManifest, RuntimeDeclaration};
use autonoetic_types::capability::Capability;
use serde_json::json;

#[derive(Debug, clap::Args)]
pub struct ImproveArgs {
    #[command(subcommand)]
    pub command: ImproveCommand,
}

#[derive(Debug, clap::Subcommand)]
pub enum ImproveCommand {
    /// Run the self-improvement loop: select sessions → diagnose → propose → validate → deploy.
    Run {
        /// Single session to improve from.
        #[arg(long, group = "source")]
        session: Option<String>,
        /// Number of most recent sessions for this agent.
        #[arg(long, group = "source", requires = "agent")]
        last_sessions: Option<usize>,
        /// Sessions since this date (RFC3339 or YYYY-MM-DD).
        #[arg(long, group = "source", requires = "agent")]
        since: Option<String>,
        /// Agent ID (required with --last-sessions or --since).
        #[arg(long)]
        agent: Option<String>,
        /// If true, diagnose + propose but stop before A/B replay.
        #[arg(long)]
        dry_run: bool,
        /// If true, refuse to deploy — output the comparison report path instead.
        #[arg(long)]
        no_prompt: bool,
    },
}

pub async fn handle_improve(config_path: &Path, command: &ImproveCommand) -> anyhow::Result<()> {
    match command {
        ImproveCommand::Run {
            session,
            last_sessions,
            since,
            agent,
            dry_run,
            no_prompt,
        } => {
            let loaded_config = autonoetic_gateway::config::load_config(config_path)?;
            let gateway_dir = loaded_config.agents_dir.join(".gateway");
            let store = Arc::new(GatewayStore::open(&gateway_dir).context(
                "Failed to open GatewayStore — has the gateway run at this path?",
            )?);

            // 1. Select sessions
            let session_ids = resolve_session_ids(&store, session.as_deref(), *last_sessions, since.as_deref(), agent.as_deref())?;
            if session_ids.is_empty() {
                anyhow::bail!("No matching sessions found");
            }

            // 2. Load session outcomes
            let outcomes: Vec<_> = session_ids
                .iter()
                .filter_map(|id| {
                    store
                        .get_session_outcome(id)
                        .ok()
                        .flatten()
                        .map(|o| (id.clone(), o))
                })
                .collect();

            // 3. Print diagnosis
            print_diagnosis(&outcomes);
            if *dry_run {
                eprintln!("[dry-run] Stopping before propose/validate as requested.");
                return Ok(());
            }

            // 4. Determine agent to improve from the first outcome
            let target_agent = outcomes
                .first()
                .map(|(_, o)| o.source_agent_id.clone())
                .or_else(|| agent.clone())
                .ok_or_else(|| anyhow::anyhow!("No agent identified from sessions or --agent flag"))?;

            // 5. Interactive issue selection
            let selected = if *no_prompt {
                // In no-prompt mode, auto-select all
                outcomes.iter().map(|(id, _)| id.clone()).collect::<Vec<_>>()
            } else {
                prompt_select_issues(&outcomes)?
            };

            if selected.is_empty() {
                eprintln!("No issues selected. Exiting.");
                return Ok(());
            }

            // 6. Validate via improvement.ab_replay
            let comparison = run_ab_replay(&loaded_config, &store, &gateway_dir, &target_agent, &selected)?;
            println!("{}", serde_json::to_string_pretty(&comparison)?);

            if comparison["status"] == "queued" {
                eprintln!(
                    "Eval runs queued. Re-run `autonoetic improve run --session <id>` once the \
                     background eval runner completes to see the comparison report."
                );
                return Ok(());
            }

            // 7. Approval
            if *no_prompt {
                eprintln!("[no-prompt] Refusing to deploy. Comparison report printed above.");
                eprintln!("[no-prompt] Run without --no-prompt to approve and deploy.");
                return Ok(());
            }

            let approved = prompt_approval(&comparison)?;
            if !approved {
                eprintln!("Deploy rejected by operator.");
                return Ok(());
            }

            // 8. Deploy: promote the candidate revision
            let candidate_ref = comparison["revision_b"].as_str().unwrap_or("candidate");
            let rev_id = candidate_ref.split('@').nth(1).unwrap_or(candidate_ref);
            let alias = AgentAliasRecord {
                alias_id: target_agent.clone(),
                agent_id: target_agent.clone(),
                revision_id: rev_id.to_string(),
                updated_at: chrono::Utc::now().to_rfc3339(),
                updated_by_type: "cli".to_string(),
                updated_by_id: "autonoetic improve".to_string(),
                reason: Some("P3 improve CLI auto-deploy".to_string()),
            };
            store
                .upsert_agent_alias(&alias)
                .context("Failed to promote revision")?;
            eprintln!("Promoted `{}` to revision `{}`.", target_agent, rev_id);
            eprintln!("To rollback: autonoetic agent revision rollback {}", target_agent);

            // 9. Monitor stub
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "ok": true,
                    "agent": target_agent,
                    "promoted_revision": rev_id,
                    "rollback_command": format!("autonoetic agent revision rollback {}", target_agent),
                    "next_steps": "Monitor the next sessions for this agent via `autonoetic session show <id>`"
                }))?
            );
        }
    }
    Ok(())
}

use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_types::agent_revision::AgentAliasRecord;

fn resolve_session_ids(
    store: &GatewayStore,
    session: Option<&str>,
    last_sessions: Option<usize>,
    since: Option<&str>,
    agent: Option<&str>,
) -> anyhow::Result<Vec<String>> {
    if let Some(sid) = session {
        return Ok(vec![sid.to_string()]);
    }

    let agent_id = agent
        .ok_or_else(|| anyhow::anyhow!("--agent required with --last-sessions or --since"))?;

    let mut session_ids = store
        .list_sessions_for_agent(agent_id)
        .with_context(|| format!("Failed to list sessions for agent '{}'", agent_id))?;

    if let Some(since_str) = since {
        let cutoff = parse_date_cutoff(since_str)?;
        session_ids.retain(|id| {
            // outcome created_at gives us the date
            store
                .get_session_outcome(id)
                .ok()
                .flatten()
                .map(|o| o.created_at >= cutoff)
                .unwrap_or(false)
        });
    }

    // Sort by most recent first (outcome updated_at)
    session_ids.sort_by(|a, b| {
        let a_time = store
            .get_session_outcome(a)
            .ok()
            .flatten()
            .map(|o| o.updated_at)
            .unwrap_or_default();
        let b_time = store
            .get_session_outcome(b)
            .ok()
            .flatten()
            .map(|o| o.updated_at)
            .unwrap_or_default();
        b_time.cmp(&a_time)
    });

    if let Some(n) = last_sessions {
        session_ids.truncate(n);
    }

    Ok(session_ids)
}

fn parse_date_cutoff(s: &str) -> anyhow::Result<String> {
    // Try RFC3339 first, then YYYY-MM-DD
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(s) {
        return Ok(dt.to_rfc3339());
    }
    if let Ok(d) = chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        let dt = d
            .and_hms_opt(0, 0, 0)
            .ok_or_else(|| anyhow::anyhow!("invalid date: {}", s))?;
        let formatted = dt.format("%Y-%m-%dT%H:%M:%S").to_string();
        return Ok(chrono::DateTime::parse_from_rfc3339(&format!("{}Z", formatted))
            .map_err(|e| anyhow::anyhow!("invalid date '{}': {}", s, e))?
            .to_rfc3339());
    }
    anyhow::bail!("Invalid date format '{}' — use RFC3339 or YYYY-MM-DD", s);
}

fn print_diagnosis(outcomes: &[(String, autonoetic_types::session_outcome::SessionOutcome)]) {
    eprintln!("── Session Diagnosis ──────────────────────────────────────");
    eprintln!("Found {} session(s) with outcomes:", outcomes.len());
    for (sid, o) in outcomes {
        let success = o.judged_success();
        let success_label = match success {
            Some(true) => "passed",
            Some(false) => "failed",
            None => "ungraded",
        };
        eprintln!(
            "  {} | agent: {} | {} | {} turns | ${:.4} | {}s wall",
            sid,
            o.source_agent_id,
            success_label,
            o.turns,
            o.cost_usd,
            o.wall_clock_secs as u64,
        );
        if let Some(ref goal) = o.task_goal {
            eprintln!("         goal: {}", goal);
        }
        if let Some(ref g) = o.grader {
            eprintln!("         grader: {} @ {}", g.grader_agent_id, g.graded_at);
            if let Some(ref ev) = g.evidence_summary {
                eprintln!("         evidence: {}", ev);
            }
        }
    }
    eprintln!("───────────────────────────────────────────────────────────");
}

fn prompt_select_issues(
    outcomes: &[(String, autonoetic_types::session_outcome::SessionOutcome)],
) -> anyhow::Result<Vec<String>> {
    eprintln!("Select sessions to address (comma-separated indices, or 'all'):");
    for (i, (sid, o)) in outcomes.iter().enumerate() {
        let success_label = match o.judged_success() {
            Some(true) => "passed",
            Some(false) => "failed",
            None => "ungraded",
        };
        eprintln!("  [{}] {} — {} — {} turns, ${:.4}", i, sid, success_label, o.turns, o.cost_usd);
    }

    let mut input = String::new();
    std::io::stdin()
        .read_line(&mut input)
        .context("Failed to read input")?;
    let input = input.trim();

    if input.eq_ignore_ascii_case("all") {
        return Ok(outcomes.iter().map(|(id, _)| id.clone()).collect());
    }

    let indices: Vec<usize> = input
        .split(',')
        .map(|s| {
            s.trim()
                .parse::<usize>()
                .map_err(|e| anyhow::anyhow!("Invalid index '{}': {}", s, e))
        })
        .collect::<anyhow::Result<Vec<_>>>()?;

    let selected: Vec<String> = indices
        .iter()
        .filter_map(|i| outcomes.get(*i).map(|(id, _)| id.clone()))
        .collect();

    if selected.is_empty() {
        anyhow::bail!("No valid sessions selected from the given indices");
    }
    Ok(selected)
}

fn run_ab_replay(
    config: &autonoetic_types::config::GatewayConfig,
    store: &Arc<GatewayStore>,
    _gateway_dir: &Path,
    agent_id: &str,
    session_ids: &[String],
) -> anyhow::Result<serde_json::Value> {
    let manifest = AgentManifest {
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
            id: "autonoetic-cli".to_string(),
            name: "Autonoetic CLI".to_string(),
            description: "CLI improve command".to_string(),
        },
        capabilities: vec![Capability::Evaluation {
            patterns: vec!["*".into()],
        }],
        llm_config: None,
        limits: None,
        background: None,
        disclosure: None,
        io: None,
        middleware: None,
        execution_mode: Default::default(),
        script_entry: None,
        script_input_mode: Default::default(),
        gateway_url: None,
        gateway_token: None,
        allowed_tool_tiers: vec![],
        agentskills_import: None,
        compression: None,
        sandbox_network: Default::default(),
    };

    let policy = autonoetic_gateway::policy::PolicyEngine::new(manifest.clone());

    // Build task_specs from session outcomes (use task_goal as message)
    let task_specs: Vec<serde_json::Value> = session_ids
        .iter()
        .map(|sid| {
            let outcome = store
                .get_session_outcome(sid)
                .ok()
                .flatten();
            let message = outcome
                .as_ref()
                .and_then(|o| o.task_goal.clone())
                .unwrap_or_else(|| format!("Replay session {}", sid));
            json!({
                "message": message,
                "case_id": sid.clone(),
            })
        })
        .collect();

    let args = json!({
        "task_specs": task_specs,
        "agent_id": agent_id,
        "revision_a": format!("{}@current", agent_id), // resolves via alias
        "revision_b": format!("{}@promoted", agent_id),
        "holdout_ratio": 0.0,
    });

    let result = AbReplayTool.execute(
        &manifest,
        &policy,
        Path::new("/tmp"),
        None,
        &args.to_string(),
        None,
        None,
        Some(config),
        Some(store.clone()),
        None,
    )?;

    serde_json::from_str(&result).map_err(|e| anyhow::anyhow!("Failed to parse tool result: {}", e))
}

fn prompt_approval(comparison: &serde_json::Value) -> anyhow::Result<bool> {
    eprintln!("\n── Comparison Report ──────────────────────────────────────");
    if let Some(summary) = comparison.get("summary") {
        eprintln!(
            "  Baseline: {}/{} passed | Candidate: {}/{} passed | Δ {}",
            summary["baseline_passed"].as_i64().unwrap_or(0),
            summary["baseline_total"].as_i64().unwrap_or(0),
            summary["candidate_passed"].as_i64().unwrap_or(0),
            summary["candidate_total"].as_i64().unwrap_or(0),
            summary["delta_passed"].as_i64().unwrap_or(0),
        );
    }
    let regressions = comparison["regressions"].as_array().map(|a| a.len()).unwrap_or(0);
    let improvements = comparison["improvements"].as_array().map(|a| a.len()).unwrap_or(0);
    if regressions > 0 {
        eprintln!("  ⚠  {} regression(s) detected", regressions);
    }
    if improvements > 0 {
        eprintln!("  ✓ {} improvement(s) detected", improvements);
    }
    eprintln!("───────────────────────────────────────────────────────────");

    eprint!("Approve deploy? [y/N]: ");
    let mut input = String::new();
    std::io::stdin()
        .read_line(&mut input)
        .context("Failed to read input")?;
    Ok(input.trim().eq_ignore_ascii_case("y"))
}
