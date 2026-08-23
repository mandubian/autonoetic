use std::io::Write;
use std::path::Path;
use std::sync::Arc;

/// Escape pipe `|` and newline characters for Markdown table cells.
fn escape_md_cell(s: &str) -> String {
    s.replace('|', "\\|").replace('\n', " ").replace('\r', " ")
}

/// Guess the GitHub `owner/repo` from the git remote origin URL.
/// Supports both `git@github.com:owner/repo.git` and `https://github.com/owner/repo.git`.
fn guess_github_repo() -> Option<String> {
    let output = std::process::Command::new("git")
        .args(["remote", "get-url", "origin"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let url = String::from_utf8_lossy(&output.stdout).trim().to_string();
    // git@github.com:owner/repo.git  →  owner/repo
    // https://github.com/owner/repo.git  →  owner/repo
    if let Some(path) = url.split("github.com").nth(1) {
        let path = path.trim_start_matches(':').trim_start_matches('/');
        let path = path.strip_suffix(".git").unwrap_or(path);
        let path = path.strip_suffix('/').unwrap_or(path);
        if let Some((owner, repo)) = path.split_once('/') {
            return Some(format!("{}/{}", owner, repo.trim_end_matches('/')));
        }
    }
    None
}

use anyhow::Context;
use autonoetic_gateway::runtime::tools::improvement::AbReplayTool;
use autonoetic_gateway::runtime::tools::NativeTool;
use autonoetic_types::agent::{AgentIdentity, AgentManifest, RuntimeDeclaration};
use autonoetic_types::agent_revision::AgentRevisionStatus;
use autonoetic_types::capability::Capability;
use autonoetic_types::id_format::mint_hashed_prefixed_id;
use serde_json::json;

#[derive(Debug, clap::Args)]
pub struct ImproveArgs {
    #[command(subcommand)]
    pub command: ImproveCommand,
}

#[derive(Debug, clap::Subcommand)]
pub enum ImproveCommand {
    /// Run the self-improvement loop: select sessions → diagnose → propose → validate → deploy.
    Run(ImproveRunArgs),
}

#[derive(Debug, clap::Args)]
#[command(group = clap::ArgGroup::new("source").required(true).multiple(true))]
pub struct ImproveRunArgs {
    /// Single session to improve from.
    #[arg(long, group = "source")]
    pub session: Option<String>,
    /// Number of most recent sessions for this agent.
    #[arg(long, group = "source", requires = "agent")]
    pub last_sessions: Option<usize>,
    /// Sessions since this date (RFC3339 or YYYY-MM-DD).
    #[arg(long, group = "source", requires = "agent")]
    pub since: Option<String>,
    /// Agent ID (required with --last-sessions or --since).
    #[arg(long)]
    pub agent: Option<String>,
    /// If true, diagnose + propose but stop before A/B replay.
    #[arg(long)]
    pub dry_run: bool,
    /// If true, refuse to deploy — output the comparison report path instead.
    #[arg(long)]
    pub no_prompt: bool,
    /// File a GitHub issue with code-level findings from failed sessions.
    /// Requires `gh` CLI installed and authenticated. Used in conjunction with --session.
    #[arg(long)]
    pub propose_code_fix: bool,
}

pub async fn handle_improve(config_path: &Path, command: &ImproveCommand) -> anyhow::Result<()> {
    match command {
        ImproveCommand::Run(args) => {
            let session = args.session.as_deref();
            let last_sessions = args.last_sessions;
            let since = args.since.as_deref();
            let agent = args.agent.as_deref();
            let dry_run = args.dry_run;
            let no_prompt = args.no_prompt;
            let propose_code_fix = args.propose_code_fix;
            let loaded_config = autonoetic_gateway::config::load_config(config_path)?;
            let gateway_dir = loaded_config.agents_dir.join(".gateway");
            let store = Arc::new(GatewayStore::open(&gateway_dir).context(
                "Failed to open GatewayStore — has the gateway run at this path?",
            )?);

            // 1. Select sessions
            let session_ids = resolve_session_ids(&store, session, last_sessions, since, agent)?;
            if session_ids.is_empty() {
                anyhow::bail!("No matching sessions found");
            }

            // 2. Load session outcomes — each must exist and be readable
            let mut outcomes: Vec<(String, _)> = Vec::new();
            for id in &session_ids {
                let outcome = store
                    .get_session_outcome(id)
                    .with_context(|| format!("Failed to read outcome for session '{}'", id))?
                    .ok_or_else(|| anyhow::anyhow!("Session '{}' has no outcome record", id))?;
                outcomes.push((id.clone(), outcome));
            }

            // 3. Print diagnosis
            print_diagnosis(&outcomes);
            if dry_run {
                eprintln!("[dry-run] Stopping before propose/validate as requested.");
                return Ok(());
            }

            // 3b. Code-fix proposal path — files a GitHub issue instead of proposing a revision
            if propose_code_fix {
                return handle_propose_code_fix(&loaded_config, &store, &gateway_dir, &session_ids, &outcomes, no_prompt).await;
            }

            // 4. Determine agent to improve from the first outcome
            let target_agent = match outcomes.first() {
                Some((_, o)) => o.source_agent_id.clone(),
                None => agent
                    .map(|a| a.to_string())
                    .ok_or_else(|| anyhow::anyhow!("No agent identified from sessions or --agent flag"))?,
            };

            // 5. Interactive issue selection
            let selected = if no_prompt {
                // In no-prompt mode, auto-select all
                outcomes.iter().map(|(id, _)| id.clone()).collect::<Vec<_>>()
            } else {
                prompt_select_issues(&outcomes)?
            };

            if selected.is_empty() {
                eprintln!("No issues selected. Exiting.");
                return Ok(());
            }

            // 6. Propose: create a candidate revision by forking the current one
            let candidate_id = propose_improvement(
                &loaded_config,
                &store,
                &gateway_dir,
                &target_agent,
                &selected,
                &outcomes,
            )?;
            eprintln!("Proposed candidate revision `{}`.", candidate_id);

            // 7. Validate via improvement.ab_replay
            let comparison = run_ab_replay(
                &loaded_config, &store, &target_agent, &selected, Some(&candidate_id),
            )?;
            println!("{}", serde_json::to_string_pretty(&comparison)?);

            if comparison["status"] == "queued" {
                eprintln!(
                    "Eval runs queued. Re-run `autonoetic improve run --session <id>` once the \
                     background eval runner completes to see the comparison report."
                );
                return Ok(());
            }

            // 8. Approval (skip prompt for L3-eligible agents)
            if no_prompt {
                eprintln!("[no-prompt] Refusing to deploy. Comparison report printed above.");
                eprintln!("[no-prompt] Run without --no-prompt to approve and deploy.");
                return Ok(());
            }

            let auto_approve = check_auto_approve_eligibility(&loaded_config, &store, &target_agent);
            let approved = if auto_approve {
                let no_regressions = comparison["regressions"]
                    .as_array()
                    .map(|a| a.is_empty())
                    .unwrap_or(false);
                let blast_radius = comparison.get("surface_change_classification")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown");
                let low_blast = blast_radius == "prompt_only" || blast_radius == "low";
                if no_regressions && low_blast {
                    eprintln!("[L3 auto-approve] Agent '{}' is L3-eligible — skipping operator prompt.", target_agent);
                    true
                } else {
                    eprintln!("[L3 auto-approve] Regressions or high blast radius — deferring to operator.");
                    prompt_approval(&comparison)?
                }
            } else {
                prompt_approval(&comparison)?
            };
            if !approved {
                eprintln!("Deploy rejected by operator.");
                return Ok(());
            }

            // 9. Deploy: promote the candidate revision
            let candidate_ref = comparison["revision_b"].as_str().unwrap_or("candidate");
            let rev_id = candidate_ref.split('@').nth(1).unwrap_or(candidate_ref);
            let candidate_eval_run_id = comparison["candidate_eval_run_id"].as_str().map(|s| s.to_string());

            let promote_manifest = promote_manifest();
            let promote_policy = autonoetic_gateway::policy::PolicyEngine::new(promote_manifest.clone());
            let promote_tool = autonoetic_gateway::runtime::tools::AgentRevisionPromoteTool;
            let promote_args = json!({
                "agent_id": target_agent,
                "revision_id": rev_id,
                "reason": "P3 improve CLI auto-deploy",
                "required_eval_run_id": candidate_eval_run_id,
            });
            let promote_output = promote_tool.execute(
                &promote_manifest,
                &promote_policy,
                Path::new("/tmp"),
                Some(&gateway_dir),
                &promote_args.to_string(),
                None,
                None,
                Some(&loaded_config),
                Some(store.clone()),
                None,
            )
            .context("Failed to promote revision via AgentRevisionPromoteTool")?;
            let promote_result: serde_json::Value = serde_json::from_str(&promote_output)
                .context("Failed to parse promotion result")?;
            eprintln!("Promoted `{}` to revision `{}`.", target_agent, rev_id);
            if let Some(prev) = promote_result["previous_revision_id"].as_str() {
                eprintln!("Previous revision was `{}`.", prev);
                eprintln!("To rollback: autonoetic agent revision promote {} {}", target_agent, prev);
            }

            // 10. Record L1 improvement cycle for P7 track record
            let regression_detected = comparison["regressions"]
                .as_array()
                .map(|a| !a.is_empty())
                .unwrap_or(false);
            let prev_id_for_cycle = promote_result["previous_revision_id"].as_str().unwrap_or("unknown");
            let cycle = autonoetic_types::improvement_cycle::ImprovementCycleRecord {
                cycle_id: uuid::Uuid::new_v4().to_string(),
                agent_id: target_agent.clone(),
                level: autonoetic_types::improvement_cycle::ImprovementLevel::L1,
                outcome: if regression_detected {
                    autonoetic_types::improvement_cycle::CycleOutcome::Regression
                } else {
                    autonoetic_types::improvement_cycle::CycleOutcome::Success
                },
                regression_detected,
                operator_decision: "approved".to_string(),
                session_id: selected.first().cloned(),
                revision_before: Some(prev_id_for_cycle.to_string()),
                revision_after: Some(rev_id.to_string()),
                blast_radius_score: None,
                created_at: chrono::Utc::now().to_rfc3339(),
                closed_at: Some(chrono::Utc::now().to_rfc3339()),
            };
            if let Err(e) = store.insert_improvement_cycle(&cycle) {
                eprintln!("[warn] Failed to record improvement cycle: {}", e);
            }

            // 10. Monitor stub
            let prev_id = promote_result["previous_revision_id"].as_str().unwrap_or("unknown");
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "ok": true,
                    "agent": target_agent,
                    "promoted_revision": rev_id,
                    "previous_revision": prev_id,
                    "rollback_command": format!("autonoetic agent revision promote {} {}", target_agent, prev_id),
                    "next_steps": "Monitor the next sessions for this agent via `autonoetic session show <id>`"
                }))?
            );
        }
    }
    Ok(())
}

use autonoetic_gateway::scheduler::gateway_store::GatewayStore;

fn check_auto_approve_eligibility(
    config: &autonoetic_types::config::GatewayConfig,
    store: &GatewayStore,
    agent_id: &str,
) -> bool {
    let improve = &config.improve;
    if !improve.auto_approve_agents.contains(&agent_id.to_string()) {
        return false;
    }

    // Constitutional hard rule: L3 never applies to agents with execution or
    // AgentSpawn capabilities, regardless of track record. Parse the SKILL.md
    // front-matter to check.
    let skill_path = config.agents_dir.join(agent_id).join("SKILL.md");
    if let Ok(content) = std::fs::read_to_string(&skill_path) {
        if let Ok((manifest, _)) = autonoetic_gateway::runtime::parser::SkillParser::parse(&content) {
            for cap in &manifest.capabilities {
                match cap {
                    autonoetic_types::capability::Capability::CodeExecution { .. }
                    | autonoetic_types::capability::Capability::ArtifactExecution
                    | autonoetic_types::capability::Capability::AgentSpawn { .. } => return false,
                    _ => {}
                }
            }
        }
    }

    store.check_automation_level_eligibility(
        agent_id,
        &autonoetic_types::improvement_cycle::ImprovementLevel::L3,
        improve.l2_threshold,
        improve.l3_threshold,
    ).unwrap_or(false)
}

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
        let mut filtered = Vec::new();
        for id in session_ids {
            let outcome = store
                .get_session_outcome(&id)
                .with_context(|| format!("Failed to read outcome for '{}'", id))?;
            if let Some(o) = outcome {
                if o.created_at >= cutoff {
                    filtered.push(id);
                }
            }
        }
        session_ids = filtered;
    }

    // Sort by most recent first (outcome updated_at) — load all outcomes upfront
    let mut with_time: Vec<(String, String)> = Vec::new();
    for sid in &session_ids {
        let outcome = store
            .get_session_outcome(sid)
            .with_context(|| format!("Failed to read outcome for sort on '{}'", sid))?
            .map(|o| o.updated_at)
            .unwrap_or_default();
        with_time.push((sid.clone(), outcome));
    }
    with_time.sort_by(|a, b| b.1.cmp(&a.1));
    session_ids = with_time.into_iter().map(|(id, _)| id).collect();

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

/// Fork the current active revision as a Candidate, tagged with the triggering
/// session IDs. Returns the short_id usable as `agent_id@rev_<short_id>`.
fn propose_improvement(
    _config: &autonoetic_types::config::GatewayConfig,
    store: &GatewayStore,
    gateway_dir: &Path,
    agent_id: &str,
    session_ids: &[String],
    outcomes: &[(String, autonoetic_types::session_outcome::SessionOutcome)],
) -> anyhow::Result<String> {
    // Resolve the currently promoted revision to fork from
    let promoted = store.resolve_alias(agent_id)
        .with_context(|| format!("Agent '{}' has no active revision — create one first", agent_id))?
        .ok_or_else(|| anyhow::anyhow!("Agent '{}' has no alias — create and promote a revision first", agent_id))?;

    let current_rev = store.get_agent_revision(&promoted.revision_id)?
        .ok_or_else(|| anyhow::anyhow!("Revision '{}' not found", promoted.revision_id))?;

    // Build a summary scoped to the SELECTED sessions only
    let selected_set: std::collections::HashSet<&str> =
        session_ids.iter().map(|s| s.as_str()).collect();
    let session_summaries: Vec<String> = outcomes.iter()
        .filter(|(id, _)| selected_set.contains(id.as_str()))
        .map(|(id, o)| {
            let label = match o.judged_success() {
                Some(true) => "passed",
                Some(false) => "failed",
                None => "ungraded",
            };
            format!("{} ({} — {})", id, label, o.task_goal.as_deref().unwrap_or("no goal"))
        })
        .collect();

    let metadata = json!({
        "proposed_by": "autonoetic improve",
        "proposed_at": chrono::Utc::now().to_rfc3339(),
        "trigger_sessions": session_ids,
        "trigger_summary": session_summaries,
        "forked_from": current_rev.revision_id,
    });

    // Generate a unique revision_id (short ref will be used for resolution)
    let revision_id = mint_hashed_prefixed_id(
        "prop-",
        &format!("{}-propose-{}", agent_id, uuid::Uuid::new_v4()),
    );

    let new_rev = autonoetic_types::agent_revision::AgentRevisionRecord {
        revision_id: revision_id.clone(),
        agent_id: agent_id.to_string(),
        base_revision_id: Some(current_rev.revision_id.clone()),
        artifact_id: current_rev.artifact_id.clone(),
        content_digest: current_rev.content_digest.clone(),
        runtime_lock_hash: current_rev.runtime_lock_hash.clone(),
        manifest_hash: current_rev.manifest_hash.clone(),
        created_at: chrono::Utc::now().to_rfc3339(),
        created_by_type: autonoetic_types::principal::PrincipalKind::Human.tag().to_string(),
        created_by_id: "autonoetic improve".to_string(),
        requested_by_type: None,
        requested_by_id: None,
        source_kind: "improvement_proposal".to_string(),
        source_ref: Some(format!("sessions:{}", session_ids.join(","))),
        origin_node_id: "gateway".to_string(),
        trust_domain: "local".to_string(),
        status: AgentRevisionStatus::Candidate,
        metadata_json: metadata,
        short_id: String::new(),
        detected_network_hosts: None,
        signature: None,
        signer_id: None,
    };

    let short_id = store.insert_agent_revision_transactional(&new_rev)
        .with_context(|| format!("Failed to insert candidate revision for '{}'", agent_id))?;

    // Copy files from the PROMOTED revision's store directory to the new
    // candidate's directory.  This guarantees the on-disk files match the
    // hashes we just stored (identical content, same digest).
    let src_dir = gateway_dir
        .join("revisions").join("agents").join(agent_id).join(&promoted.revision_id);
    let dst_dir = gateway_dir
        .join("revisions").join("agents").join(agent_id).join(&revision_id);

    std::fs::create_dir_all(&dst_dir)
        .with_context(|| format!("Failed to create revision directory {:?}", dst_dir))?;

    let skill_src = src_dir.join("SKILL.md");
    let lock_src = src_dir.join("runtime.lock");

    anyhow::ensure!(
        skill_src.exists(),
        "Source SKILL.md not found at {:?} — cannot fork revision",
        skill_src
    );
    anyhow::ensure!(
        lock_src.exists(),
        "Source runtime.lock not found at {:?} — cannot fork revision",
        lock_src
    );

    std::fs::copy(&skill_src, dst_dir.join("SKILL.md"))
        .with_context(|| format!("Failed to copy SKILL.md to {:?}", dst_dir))?;
    std::fs::copy(&lock_src, dst_dir.join("runtime.lock"))
        .with_context(|| format!("Failed to copy runtime.lock to {:?}", dst_dir))?;

    Ok(short_id)
}

/// File a GitHub issue with code-level findings from one or more failed sessions.
/// Requires `gh` CLI to be installed and authenticated.
async fn handle_propose_code_fix(
    config: &autonoetic_types::config::GatewayConfig,
    store: &Arc<GatewayStore>,
    gateway_dir: &Path,
    _session_ids: &[String],
    outcomes: &[(String, autonoetic_types::session_outcome::SessionOutcome)],
    _no_prompt: bool,
) -> anyhow::Result<()> {
    use autonoetic_gateway::runtime::tools::github_issue::GithubIssueCreateTool;
    use autonoetic_types::causal_chain::CausalEventRecord;

    let target_agent = outcomes
        .first()
        .map(|(_, o)| o.source_agent_id.clone())
        .unwrap_or_default();

    // Build shared manifest, policy, and tool once (constant across sessions)
    let manifest = AgentManifest {
        remote_access: None,
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
            description: "CLI code-issue-proposer".to_string(),
            singleton: false,
            resident_idle_ttl_secs: None,
        },
        capabilities: vec![Capability::GithubIssueCreate {
            patterns: vec!["*".into()],
        }],
        llm_overrides: None,
        llm_preset: None,
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
            excluded_tools: vec![],
            sections: Vec::new(),
        agentskills_import: None,
        compression: None,
            open_web: false,
        sandbox_network: autonoetic_types::agent::SandboxNetworkPolicy::default(),
        egress: None,
        };
    let policy = autonoetic_gateway::policy::PolicyEngine::new(manifest.clone());
    let tool = GithubIssueCreateTool;

    for (sid, outcome) in outcomes {
        // Skip passed sessions
        if outcome.judged_success() == Some(true) {
            eprintln!("[skip] Session {} passed — no issue filed.", sid);
            continue;
        }

        // Gather causal events for this session
        let causal_events: Vec<CausalEventRecord> = store
            .search_causal_events(Some(sid), None, 100)
            .with_context(|| format!("Failed to read causal events for session '{}'", sid))?;

        let tool_failures: Vec<&CausalEventRecord> = causal_events
            .iter()
            .filter(|e| e.status == "ERROR" || e.status == "DENIED")
            .collect();

        // Build issue body
        let mut body = String::new();

        body.push_str(&format!(
            "## Session `{}`\n\n**Agent:** {}  \n**Goal:** {}  \n**Status:** {}  \n**Turns:** {}  \n**Cost:** ${:.4}\n\n",
            sid,
            outcome.source_agent_id,
            outcome.task_goal.as_deref().unwrap_or("(no goal)"),
            match outcome.judged_success() {
                Some(true) => "passed",
                Some(false) => "failed",
                None => "ungraded",
            },
            outcome.turns,
            outcome.cost_usd,
        ));

        if let Some(ref grader) = outcome.grader {
            if let Some(ref summary) = grader.evidence_summary {
                body.push_str(&format!("**Evidence:** {}\n\n", escape_md_cell(summary)));
            }
        }

        if !tool_failures.is_empty() {
            body.push_str("### Tool Failures / Denials\n\n");
            body.push_str("| Action | Status | Reason |\n");
            body.push_str("|--------|--------|--------|\n");
            for ev in tool_failures.iter().take(10) {
                let reason = ev.reason.as_deref().map(escape_md_cell).unwrap_or_else(|| "(none)".to_string());
                body.push_str(&format!("| `{}` | {} | {} |\n", ev.action, ev.status, reason));
            }
            body.push('\n');
        }

        body.push_str("### Suggested Fix Area\n\n");
        body.push_str("_This issue was auto-filed by the code-issue-proposer. Review the causal event log ");
        body.push_str("and digest to identify the root cause in gateway code._\n\n");

        body.push_str("#### Reproduction\n");
        body.push_str(&format!("1. Run `autonoetic session trace {}`\n", sid));
        body.push_str(&format!("2. Review the outcome digest at session `{}`\n", sid));
        body.push_str("3. Identify the failing tool call or schema mismatch\n");

        let title = format!(
            "[code-issue-proposer] Session {} — {}",
            sid,
            outcome.task_goal.as_deref().unwrap_or("code-level issue")
        );

        let repo = guess_github_repo()
            .ok_or_else(|| anyhow::anyhow!("Cannot determine GitHub repo from git remote — set origin or pass --repo"))?;

        let tool_args = serde_json::json!({
            "title": title,
            "body": body,
            "labels": "code-issue-proposer",
            "repo": repo,
        });

        let result = tool
            .execute(
                &manifest,
                &policy,
                Path::new("/tmp"),
                Some(gateway_dir),
                &tool_args.to_string(),
                Some(sid),
                None,
                Some(config),
                Some(store.clone()),
                None,
            )
            .with_context(|| format!("Failed to file GitHub issue for session '{}'", sid))?;

        let parsed: serde_json::Value = serde_json::from_str(&result)
            .with_context(|| format!("Failed to parse tool output: {}", result))?;

        let url = parsed["url"].as_str().unwrap_or("(unknown)");
        eprintln!("Filed issue for session `{}`: {}", sid, url);

        // Emit causal event for audit trail
        let _ = store.create_causal_event(&autonoetic_types::causal_chain::CausalEventRecord {
            event_id: uuid::Uuid::new_v4().to_string(),
            agent_id: target_agent.clone(),
            session_id: sid.to_string(),
            turn_id: None,
            event_seq: chrono::Utc::now().timestamp_millis().max(0) as u64,
            timestamp: chrono::Utc::now().to_rfc3339(),
            category: "tool".to_string(),
            action: "code_issue_proposed".to_string(),
            status: "SUCCESS".to_string(),
            enforced_rules: autonoetic_types::causal_chain::default_enforced_rules(),
            target: Some(url.to_string()),
            payload: Some(serde_json::json!({
                "session_id": sid,
                "agent_id": target_agent.clone(),
                "issue_url": url,
            }).to_string()),
            payload_ref: None,
            evidence_ref: None,
            reason: Some("Auto-filed by code-issue-proposer via CLI".to_string()),
        });
    }

    Ok(())
}

fn run_ab_replay(
    config: &autonoetic_types::config::GatewayConfig,
    store: &Arc<GatewayStore>,
    agent_id: &str,
    session_ids: &[String],
    explicit_candidate: Option<&str>,
) -> anyhow::Result<serde_json::Value> {
    let manifest = AgentManifest {
        remote_access: None,
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
            singleton: false,
            resident_idle_ttl_secs: None,
        },
        capabilities: vec![Capability::Evaluation {
            patterns: vec!["*".into()],
        }],
        llm_overrides: None,
        llm_preset: None,
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
            excluded_tools: vec![],
            sections: Vec::new(),
        agentskills_import: None,
        compression: None,
            open_web: false,
        sandbox_network: Default::default(),
        egress: None,
        };

    let policy = autonoetic_gateway::policy::PolicyEngine::new(manifest.clone());

    // Build task_specs from session outcomes (use task_goal as message)
    let mut task_specs: Vec<serde_json::Value> = Vec::new();
    for sid in session_ids {
        let outcome = store
            .get_session_outcome(sid)
            .with_context(|| format!("Failed to read outcome for '{}'", sid))?;
        let message = outcome
            .as_ref()
            .and_then(|o| o.task_goal.clone())
            .unwrap_or_else(|| format!("Replay session {}", sid));
        task_specs.push(json!({
            "message": message,
            "case_id": sid.clone(),
        }));
    }

    // revision_a = plain agent_id resolves to the currently promoted revision
    // revision_b = explicit candidate (from propose step) or fallback to alias lookup
    let revision_b = if let Some(cand) = explicit_candidate {
        format!("{}@{}", agent_id, cand)
    } else {
        // Fallback: resolve the latest non-promoted candidate revision
        let revisions = store.list_agent_revisions(agent_id)
            .with_context(|| format!("Failed to list revisions for '{}'", agent_id))?;
        let promoted = store.resolve_alias(agent_id)
            .with_context(|| format!("Failed to resolve alias for '{}'", agent_id))?;
        let promoted_rev = promoted.as_ref().map(|a| a.revision_id.as_str());
        let candidate_rev = revisions
            .iter()
            .find(|r| {
                Some(r.revision_id.as_str()) != promoted_rev
                    && matches!(r.status, AgentRevisionStatus::Candidate | AgentRevisionStatus::Ready)
            })
            .map(|r| r.revision_id.as_str());

        match candidate_rev {
            Some(cand) => format!("{}@{}", agent_id, cand),
            None => {
                eprintln!("[warn] No candidate revision found for '{}' — comparing against itself", agent_id);
                agent_id.to_string()
            }
        }
    };
    let args = json!({
        "task_specs": task_specs,
        "agent_id": agent_id,
        "revision_a": agent_id,
        "revision_b": revision_b,
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

/// Manifest with AgentRevision capability, used for the promote step.
fn promote_manifest() -> AgentManifest {
    AgentManifest {
        remote_access: None,
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
            singleton: false,
            resident_idle_ttl_secs: None,
        },
        capabilities: vec![Capability::AgentRevision {
            patterns: vec!["*".into()],
        }],
        llm_overrides: None,
        llm_preset: None,
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
            excluded_tools: vec![],
            sections: Vec::new(),
        agentskills_import: None,
        compression: None,
            open_web: false,
        sandbox_network: Default::default(),
        egress: None,
        }
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
    std::io::stderr().flush().context("Failed to flush stderr")?;
    let mut input = String::new();
    std::io::stdin()
        .read_line(&mut input)
        .context("Failed to read input")?;
    Ok(input.trim().eq_ignore_ascii_case("y"))
}
