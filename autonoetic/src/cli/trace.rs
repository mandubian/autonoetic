use std::io::{BufRead, BufReader as StdBufReader};
use std::path::Path;

use super::common::{AgentTrace, SessionSummary};
use autonoetic_gateway::llm::Message;
use autonoetic_types::background::UserInteraction;
use autonoetic_types::causal_chain::{CausalChainEntry, CausalEventRecord, EntryStatus};
use autonoetic_types::workflow::{
    TaskRun, TaskRunStatus, WorkflowEventRecord, WorkflowRun, WorkflowRunStatus,
};
use serde::Serialize;

/// ANSI color helpers for terminal output.
mod color {
    pub const RESET: &str = "\x1b[0m";
    pub const BOLD: &str = "\x1b[1m";
    pub const DIM: &str = "\x1b[2m";
    pub const RED: &str = "\x1b[31m";
    pub const GREEN: &str = "\x1b[32m";
    pub const YELLOW: &str = "\x1b[33m";
    pub const BLUE: &str = "\x1b[34m";
    pub const MAGENTA: &str = "\x1b[35m";
    pub const CYAN: &str = "\x1b[36m";
    pub const WHITE: &str = "\x1b[37m";
    pub const BRIGHT_RED: &str = "\x1b[91m";
    pub const BRIGHT_YELLOW: &str = "\x1b[93m";
    pub const BRIGHT_BLUE: &str = "\x1b[94m";
    pub const BRIGHT_CYAN: &str = "\x1b[96m";

    pub fn status_color(s: &str) -> &'static str {
        match s {
            "SUCCESS" => GREEN,
            "DENIED" => YELLOW,
            "ERROR" => RED,
            _ => WHITE,
        }
    }

    pub fn status_label(s: &str) -> String {
        let c = status_color(s);
        format!("{}{}{}{}", c, BOLD, s, RESET)
    }

    pub fn agent(s: &str) -> String {
        format!("{}{}{}{}", BRIGHT_CYAN, BOLD, s, RESET)
    }

    pub fn category(s: &str) -> String {
        match s {
            "tool_invoke" => format!("{}{}{}{}", MAGENTA, BOLD, s, RESET),
            "gateway" => format!("{}{}{}{}", BLUE, BOLD, s, RESET),
            "lifecycle" => format!("{}{}{}{}", CYAN, BOLD, s, RESET),
            "artifact" => format!("{}{}{}{}", YELLOW, BOLD, s, RESET),
            "llm" => format!("{}{}{}{}", BRIGHT_BLUE, BOLD, s, RESET),
            _ => format!("{}{}{}", DIM, s, RESET),
        }
    }

    pub fn action(s: &str) -> String {
        match s {
            "requested" | "started" => format!("{}{}{}", CYAN, s, RESET),
            "completed" | "success" => format!("{}{}{}", GREEN, s, RESET),
            "error" | "failed" => format!("{}{}{}{}", BRIGHT_RED, BOLD, s, RESET),
            "denied" => format!("{}{}{}{}", YELLOW, BOLD, s, RESET),
            _ => s.to_string(),
        }
    }

    pub fn tool_name(s: &str) -> String {
        format!("{}{}{}{}", BRIGHT_YELLOW, BOLD, s, RESET)
    }

    pub fn seq(s: u64) -> String {
        format!("{}{}{}", DIM, s, RESET)
    }

    pub fn separator(len: usize) -> String {
        format!("{}{}{}", DIM, "─".repeat(len), RESET)
    }

    pub fn dim(s: &str) -> String {
        format!("{}{}{}", DIM, s, RESET)
    }
}

pub fn handle_trace_sessions(
    config_path: &Path,
    requested_agent: Option<&str>,
    json_output: bool,
) -> anyhow::Result<()> {
    // Preferred source: the gateway's causal_events table over RPC (same as
    // `trace show`). The file reader below predates #1119 and finds nothing
    // now that events live in gateway.db — it stays only as an offline
    // fallback for pre-#1119 workspaces.
    let db_result = load_session_summaries_from_db(config_path, requested_agent);
    if let Ok(summaries) = &db_result {
        if !summaries.is_empty() {
            return render_session_summaries(summaries, json_output);
        }
    }

    let traces = load_agent_traces(config_path, requested_agent)?;
    let sessions = super::common::collect_session_summaries(&traces);
    if sessions.is_empty() {
        // Don't dress a gateway/RPC failure up as "no data": when the DB
        // path errored and the offline fallback has nothing either, the RPC
        // error is the actionable one ("is it running?").
        if let Err(e) = db_result {
            return Err(e);
        }
    }
    render_session_summaries(&sessions, json_output)
}

fn render_session_summaries(sessions: &[SessionSummary], json_output: bool) -> anyhow::Result<()> {
    let sessions = sessions;
    if json_output {
        let body = sessions
            .iter()
            .map(|s| {
                serde_json::json!({
                    "agent_id": s.agent_id,
                    "session_id": s.session_id,
                    "first_timestamp": s.first_timestamp,
                    "last_timestamp": s.last_timestamp,
                    "event_count": s.event_count,
                    "max_event_seq": s.max_event_seq
                })
            })
            .collect::<Vec<_>>();
        println!("{}", serde_json::to_string_pretty(&body)?);
        return Ok(());
    }

    if sessions.is_empty() {
        println!("No trace sessions found.");
        return Ok(());
    }

    println!(
        "{}{}{:<30} {:<38} {:<26} {:<26} {:<8} {:<10}{}",
        color::DIM,
        color::BOLD,
        "AGENT",
        "SESSION ID",
        "FIRST TS",
        "LAST TS",
        "EVENTS",
        "MAX SEQ",
        color::RESET
    );
    println!("{}", color::separator(146));
    for s in sessions {
        println!(
            "{} {:<38} {:<26} {:<26} {}{} {}",
            color::agent(&s.agent_id),
            s.session_id,
            s.first_timestamp,
            s.last_timestamp,
            color::BRIGHT_YELLOW,
            s.event_count,
            color::RESET,
        );
    }
    Ok(())
}

/// Session listing from the gateway database (`trace.sessions` RPC).
///
/// The response is deserialized strictly: a missing or mistyped field is a
/// schema mismatch against the running gateway and should fail loudly, not
/// render as blank rows.
fn load_session_summaries_from_db(
    config_path: &Path,
    requested_agent: Option<&str>,
) -> anyhow::Result<Vec<SessionSummary>> {
    #[derive(serde::Deserialize)]
    struct SessionSummaryRow {
        agent_id: String,
        session_id: String,
        first_timestamp: String,
        last_timestamp: String,
        event_count: u64,
        max_event_seq: u64,
    }

    let config = autonoetic_gateway::config::load_config(config_path)?;
    let rpc = crate::cli::rpc::GatewayRpc::from_config(&config)?;
    let result = rpc.call(
        "trace.sessions",
        serde_json::json!({ "agent_id": requested_agent }),
    )?;
    let rows: Vec<SessionSummaryRow> = serde_json::from_value(result)?;
    Ok(rows
        .into_iter()
        .map(|r| SessionSummary {
            agent_id: r.agent_id,
            session_id: r.session_id,
            first_timestamp: r.first_timestamp,
            last_timestamp: r.last_timestamp,
            event_count: r.event_count as usize,
            max_event_seq: r.max_event_seq,
        })
        .collect())
}

pub fn handle_trace_session(
    config_path: &Path,
    session_id: &str,
    requested_agent: Option<&str>,
    json_output: bool,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        !session_id.trim().is_empty(),
        "session_id must not be empty"
    );

    let user_interactions =
        load_trace_user_interactions(config_path, session_id).unwrap_or_default();

    // Try to load from gateway database first (preferred method)
    let db_result = load_traces_from_db(
        config_path,
        Some(session_id),
        requested_agent,
        1000, // Default limit
    );

    if let Ok(ref db_events) = db_result {
        if !db_events.is_empty() {
            return handle_trace_session_from_db(
                session_id,
                db_events,
                &user_interactions,
                json_output,
            );
        }
    }

    if !user_interactions.is_empty() {
        return handle_trace_session_from_db(session_id, &[], &user_interactions, json_output);
    }

    // Fall back to JSONL files
    let traces = load_agent_traces(config_path, requested_agent)?;
    let mut matches: Vec<(String, Vec<CausalChainEntry>)> = Vec::new();
    for trace in traces {
        let events = trace
            .entries
            .into_iter()
            .filter(|entry| entry.session_id == session_id)
            .collect::<Vec<_>>();
        if !events.is_empty() {
            matches.push((trace.agent_id, events));
        }
    }

    anyhow::ensure!(
        !matches.is_empty(),
        "No events found for session '{}'{}",
        session_id,
        requested_agent
            .map(|a| format!(" under agent '{}'", a))
            .unwrap_or_default()
    );
    if requested_agent.is_none() && matches.len() > 1 {
        let owners = matches
            .iter()
            .map(|(agent_id, _)| agent_id.clone())
            .collect::<Vec<_>>()
            .join(", ");
        anyhow::bail!(
            "Session '{}' found in multiple agents ({}). Re-run with --agent.",
            session_id,
            owners
        );
    }

    let (agent_id, mut entries) = matches
        .into_iter()
        .next()
        .ok_or_else(|| anyhow::anyhow!("failed to resolve session match"))?;
    entries.sort_by(|a, b| {
        a.timestamp
            .cmp(&b.timestamp)
            .then_with(|| a.event_seq.cmp(&b.event_seq))
    });

    let user_interactions =
        load_trace_user_interactions(config_path, session_id).unwrap_or_default();

    if json_output {
        let body = serde_json::json!({
            "agent_id": agent_id,
            "session_id": session_id,
            "events": entries,
            "user_interactions": user_interactions,
        });
        println!("{}", serde_json::to_string_pretty(&body)?);
        return Ok(());
    }

    println!("Agent: {}", color::agent(&agent_id));
    println!(
        "Session: {}",
        color::BRIGHT_YELLOW.to_string() + session_id + color::RESET
    );
    println!(
        "{}{}{:<8} {:<24} {:<15} {:<18} {:<15} {:<20} {}{}",
        color::DIM,
        color::BOLD,
        "SEQ",
        "TIMESTAMP",
        "CATEGORY",
        "ACTION",
        "STATUS",
        "TARGET",
        "REASON",
        color::RESET
    );
    println!("{}", color::separator(130));
    for entry in entries {
        let target_str = entry.target.as_deref().unwrap_or("-");
        let reason_str = entry.reason.as_deref().unwrap_or("-");
        let target_display = if target_str.len() > 19 {
            format!("{}…", &target_str[..18])
        } else {
            target_str.to_string()
        };
        let reason_display = if reason_str.len() > 35 {
            format!("{}…", &reason_str[..34])
        } else {
            reason_str.to_string()
        };

        // Highlight reason in red for errors/denials, dim otherwise
        let reason_colored = match &entry.status {
            EntryStatus::Error => format!(
                "{}{}{}{}",
                color::BRIGHT_RED,
                color::BOLD,
                reason_display,
                color::RESET
            ),
            EntryStatus::Denied => format!(
                "{}{}{}{}",
                color::YELLOW,
                color::BOLD,
                reason_display,
                color::RESET
            ),
            _ => color::dim(&reason_display),
        };

        println!(
            "{} {:<24} {} {} {} {} {}",
            color::seq(entry.event_seq),
            entry.timestamp,
            color::category(&entry.category),
            color::action(&entry.action),
            color::status_label(&format!("{:?}", entry.status)),
            color::dim(&target_display),
            reason_colored,
        );

        // Show tool-specific info for tool_invoke events
        if entry.category == "tool_invoke" {
            if let Some(ref payload) = entry.payload {
                if let Some(tool_name) = payload.get("tool_name").and_then(|v| v.as_str()) {
                    let args_preview = payload
                        .get("arguments")
                        .and_then(|v| v.as_str())
                        .map(|a| {
                            if a.len() > 80 {
                                format!("{}…", &a[..79])
                            } else {
                                a.to_string()
                            }
                        })
                        .unwrap_or_default();
                    let result_preview = payload
                        .get("result_preview")
                        .and_then(|v| v.as_str())
                        .map(|r| {
                            if r.len() > 80 {
                                format!("{}…", &r[..79])
                            } else {
                                r.to_string()
                            }
                        })
                        .unwrap_or_default();

                    if entry.action == "requested" && !args_preview.is_empty() {
                        println!(
                            "      {}├─ {}({}){}",
                            color::DIM,
                            color::tool_name(tool_name),
                            color::dim(&args_preview),
                            color::RESET
                        );
                    } else if entry.action == "completed" && !result_preview.is_empty() {
                        println!(
                            "      {}├─ {} → {}{}",
                            color::DIM,
                            color::tool_name(tool_name),
                            color::dim(&result_preview),
                            color::RESET
                        );
                    } else {
                        println!(
                            "      {}├─ {}{}",
                            color::DIM,
                            color::tool_name(tool_name),
                            color::RESET
                        );
                    }
                }
            }
        } else {
            if let Some(ref payload) = entry.payload {
                let payload_str = serde_json::to_string(payload).unwrap_or_default();
                if payload_str.len() > 2 && payload_str != "null" {
                    let truncated = if payload_str.len() > 120 {
                        format!("{}…", &payload_str[..119])
                    } else {
                        payload_str
                    };
                    println!(
                        "      {}├─ payload: {}{}{}",
                        color::DIM,
                        color::BRIGHT_BLUE,
                        truncated,
                        color::RESET
                    );
                }
            }
        }
    }

    print_user_interactions_trace_section(&user_interactions);
    Ok(())
}

pub fn handle_trace_event(
    config_path: &Path,
    log_id: &str,
    requested_agent: Option<&str>,
    json_output: bool,
) -> anyhow::Result<()> {
    anyhow::ensure!(!log_id.trim().is_empty(), "log_id must not be empty");
    let traces = load_agent_traces(config_path, requested_agent)?;
    let mut matches: Vec<(String, CausalChainEntry)> = Vec::new();
    for trace in traces {
        for entry in trace.entries {
            if entry.log_id == log_id {
                matches.push((trace.agent_id.clone(), entry));
            }
        }
    }

    anyhow::ensure!(
        !matches.is_empty(),
        "No event found for log_id '{}'{}",
        log_id,
        requested_agent
            .map(|a| format!(" under agent '{}'", a))
            .unwrap_or_default()
    );
    if requested_agent.is_none() && matches.len() > 1 {
        let owners = matches
            .iter()
            .map(|(agent_id, _)| agent_id.clone())
            .collect::<Vec<_>>()
            .join(", ");
        anyhow::bail!(
            "Event '{}' found in multiple agents ({}). Re-run with --agent.",
            log_id,
            owners
        );
    }

    let (agent_id, entry) = matches
        .into_iter()
        .next()
        .ok_or_else(|| anyhow::anyhow!("failed to resolve event match"))?;

    if json_output {
        let body = serde_json::json!({
            "agent_id": agent_id,
            "event": entry,
        });
        println!("{}", serde_json::to_string_pretty(&body)?);
        return Ok(());
    }

    println!("Agent: {}", agent_id);
    println!("{}", serde_json::to_string_pretty(&entry)?);
    Ok(())
}

/// Print `post_session_narrative.md` from the content store for a session's root id.
pub fn handle_trace_digest(
    config_path: &Path,
    session_id: &str,
    json_output: bool,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        !session_id.trim().is_empty(),
        "session_id must not be empty"
    );
    let config = autonoetic_gateway::config::load_config(config_path)?;
    let gateway_dir = autonoetic_gateway::execution::gateway_root_dir(&config);
    let base = autonoetic_gateway::runtime::live_digest::base_session_id(session_id.trim());
    let cs = autonoetic_gateway::runtime::content_store::ContentStore::new(&gateway_dir)?;
    let name =
        autonoetic_gateway::runtime::post_session_digest::POST_SESSION_NARRATIVE_CONTENT_NAME;
    let bytes = cs
        .read_by_name(base, name)
        .map_err(|_| {
            anyhow::anyhow!(
                "No post-session narrative for root session '{}'. Is digest_agent enabled and did a session complete with enough turns?",
                base
            )
        })?;
    let text = String::from_utf8(bytes)
        .map_err(|e| anyhow::anyhow!("post_session_narrative.md is not valid UTF-8: {e}"))?;
    if json_output {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "root_session_id": base,
                "name": name,
                "text": text,
            }))?
        );
        return Ok(());
    }
    println!(
        "{}Post-session narrative{} {}{}{}",
        color::BOLD,
        color::RESET,
        color::DIM,
        base,
        color::RESET
    );
    println!("{}", text);
    Ok(())
}

/// `autonoetic trace contract-health` — the standing contract-health view
/// (#302). Tallies how often each constitutional clause (principle/right) has
/// been enforced, sourced from the `enforced_rules` carried on causal events
/// and attributed to clauses via the enforcement register.
pub fn handle_trace_contract_health(
    config_path: &Path,
    since: Option<&str>,
    json_output: bool,
) -> anyhow::Result<()> {
    let config = autonoetic_gateway::config::load_config(config_path)?;
    let rpc = crate::cli::rpc::GatewayRpc::from_config(&config)?;
    // The full snapshot is computed server-side (store + enforcement
    // register) — the CLI only renders (#1119 tranche 6).
    let body = rpc.call(
        "trace.contract_health",
        serde_json::json!({ "since": since }),
    )?;

    let dead: Vec<&str> = body["dead_clauses"]
        .as_array()
        .map(|a| a.iter().filter_map(|c| c.as_str()).collect())
        .unwrap_or_default();
    let registered_count = body["registered_clause_count"].as_u64().unwrap_or(0);
    let by_clause: Vec<(String, u64)> = body["by_clause"]
        .as_array()
        .map(|a| {
            a.iter()
                .map(|e| {
                    (
                        e["clause"].as_str().unwrap_or("?").to_string(),
                        e["count"].as_u64().unwrap_or(0),
                    )
                })
                .collect()
        })
        .unwrap_or_default();
    let unattributed = body["unattributed"].as_u64().unwrap_or(0);
    let by_clause_index: std::collections::HashMap<String, serde_json::Value> = body["by_clause"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|e| {
            e["clause"]
                .as_str()
                .map(|c| (c.to_string(), e.clone()))
        })
        .collect();
    let leak_summary = body["discretion_leaks"].as_array().cloned().unwrap_or_default();

    if json_output {
        println!("{}", serde_json::to_string_pretty(&body)?);
        return Ok(());
    }

    println!(
        "{}Contract health{} {}",
        color::BOLD,
        color::RESET,
        match since {
            Some(ts) => color::dim(&format!("(since {ts})")),
            None => color::dim("(all retained events)"),
        }
    );
    println!();

    if by_clause.is_empty() {
        println!(
            "{}No clause enforcements recorded.{}",
            color::DIM,
            color::RESET
        );
    } else {
        println!(
            "{}{}{:<10} {:<8} {:<8} {}{}",
            color::DIM,
            color::BOLD,
            "CLAUSE",
            "COUNT",
            "BINDS",
            "TITLE",
            color::RESET
        );
        println!("{}", color::separator(72));
        for (clause, count) in &by_clause {
            let title = clause_title_from(&by_clause_index, clause);
            let binds = clause_binds_from(&by_clause_index, clause);
            println!(
                "{}{:<10}{} {}{:<8}{} {:<8} {}",
                color::BRIGHT_CYAN,
                clause,
                color::RESET,
                color::BRIGHT_YELLOW,
                count,
                color::RESET,
                binds,
                title,
            );
        }
    }

    if unattributed > 0 {
        println!();
        println!(
            "{}{} unattributed enforcement(s){} — rule/right IDs not yet in the register (migration gap).",
            color::YELLOW,
            unattributed,
            color::RESET
        );
    }

    println!();
    if dead.is_empty() {
        println!(
            "{}Every registered clause ({}) fired at least once in this window.{}",
            color::DIM,
            registered_count,
            color::RESET
        );
    } else {
        println!(
            "{}Never enforced in window{} ({} of {} registered clauses):",
            color::BOLD,
            color::RESET,
            dead.len(),
            registered_count
        );
        for clause in &dead {
            let title = clause_title_from(&by_clause_index, clause);
            println!("  {}{:<10}{} {}", color::BRIGHT_CYAN, clause, color::RESET, title);
        }
        println!(
            "{}Scoped to clauses migrated into the structured enforcement register — not the full constitution.{}",
            color::DIM,
            color::RESET
        );
    }

    // #771 D.3: "Top leaks this window" — the standing agenda the steward
    // office drafts amendments against.
    println!();
    if leak_summary.is_empty() {
        println!(
            "{}No discretion leaks recorded (the gateway did not exercise judgment reserved to the agent).{}",
            color::DIM,
            color::RESET
        );
    } else {
        println!(
            "{}Top discretion leaks{} {} — gateway judgment reserved to the agent or pre-committed law:",
            color::BOLD,
            color::RESET,
            match since {
                Some(ts) => color::dim(&format!("(since {ts})")),
                None => color::dim("(all retained events)"),
            }
        );
        println!(
            "{}{:<10} {:<30} {:<8}{}{}",
            color::DIM,
            color::BOLD,
            "RULE",
            "KIND",
            "COUNT",
            color::RESET,
        );
        println!("{}", color::separator(72));
        for t in &leak_summary {
            println!(
                "{}{:<10}{} {:<30} {}{:<8}{}",
                color::BRIGHT_YELLOW,
                t["rule_id"].as_str().unwrap_or("?"),
                color::RESET,
                t["kind"].as_str().unwrap_or("?"),
                color::BRIGHT_YELLOW,
                t["count"].as_u64().unwrap_or(0),
                color::RESET,
            );
        }
    }
    Ok(())
}

/// Look up a clause's registered title from the contract-health payload.
fn clause_title_from(
    rows: &std::collections::HashMap<String, serde_json::Value>,
    clause: &str,
) -> String {
    rows.get(clause)
        .and_then(|e| e["title"].as_str())
        .unwrap_or("<unknown>")
        .to_string()
}

/// Look up a clause's binds label from the contract-health payload.
fn clause_binds_from(
    rows: &std::collections::HashMap<String, serde_json::Value>,
    clause: &str,
) -> String {
    rows.get(clause)
        .and_then(|e| e["binds"].as_str())
        .unwrap_or("-")
        .to_string()
}

/// `autonoetic trace civic-health` — the standing civic-health view (#772
/// E.2). The dual of contract-health: contract-health measures whether the
/// *gateway* honors the law, civic-health measures whether *agents* use it.
/// Tallies each agent's constitutional proposals and anomaly flags, filed vs
/// still-pending, so both that agents exercise voice/witnessing and whether
/// those are being answered are visible in one view.
pub fn handle_trace_civic_health(
    config_path: &Path,
    since: Option<&str>,
    json_output: bool,
) -> anyhow::Result<()> {
    let config = autonoetic_gateway::config::load_config(config_path)?;
    let rpc = crate::cli::rpc::GatewayRpc::from_config(&config)?;
    // Aggregated server-side; the CLI only renders (#1119 tranche 6).
    let body = rpc.call(
        "trace.civic_health",
        serde_json::json!({ "since": since }),
    )?;
    let by_agent = body["by_agent"].as_array().cloned().unwrap_or_default();

    if json_output {
        println!("{}", serde_json::to_string_pretty(&body)?);
        return Ok(());
    }

    println!(
        "{}Civic health{} {}",
        color::BOLD,
        color::RESET,
        match since {
            Some(ts) => color::dim(&format!("(since {ts})")),
            None => color::dim("(all retained items)"),
        }
    );
    println!();

    if by_agent.is_empty() {
        println!(
            "{}No civic activity recorded (no constitutional proposals or anomaly flags filed).{}",
            color::DIM,
            color::RESET
        );
    } else {
        println!(
            "{}{}{:<28} {:<20} {:<20} {}{}",
            color::DIM,
            color::BOLD,
            "AGENT",
            "PROPOSALS(pending)",
            "FLAGS(pending)",
            "INVITATIONS(open/answered)",
            color::RESET
        );
        println!("{}", color::separator(72));
        for e in &by_agent {
            println!(
                "{}{:<28}{} {} ({} pending)   {} ({} pending)   {} ({} / {} answered)",
                color::BRIGHT_CYAN,
                e["agent_id"].as_str().unwrap_or("?"),
                color::RESET,
                e["proposals_filed"].as_u64().unwrap_or(0),
                e["proposals_pending"].as_u64().unwrap_or(0),
                e["flags_filed"].as_u64().unwrap_or(0),
                e["flags_pending"].as_u64().unwrap_or(0),
                e["invitations_issued"].as_u64().unwrap_or(0),
                e["invitations_open"].as_u64().unwrap_or(0),
                e["invitations_answered"].as_u64().unwrap_or(0),
            );
        }
    }

    println!();
    println!(
        "{}Precision-of-adjudication metrics are not yet tracked here (RFC E.1/E.3).{}",
        color::DIM,
        color::RESET
    );
    Ok(())
}

pub fn load_agent_traces(
    config_path: &Path,
    requested_agent: Option<&str>,
) -> anyhow::Result<Vec<AgentTrace>> {
    let config = autonoetic_gateway::config::load_config(config_path)?;
    let repo = autonoetic_gateway::AgentRepository::from_config(&config);

    let filtered: Vec<_> = if let Some(agent_id) = requested_agent {
        let loaded = repo.load_unvetted_from_ingest_dir(agent_id)?;
        vec![loaded]
    } else {
        repo.list_loaded_unvetted_from_ingest_dir()?
    };

    let mut traces = Vec::new();
    for agent in filtered {
        let path = agent.dir.join("history").join("causal_chain.jsonl");
        if !path.exists() {
            continue;
        }
        let entries = read_trace_entries(&path)?;
        traces.push(AgentTrace {
            agent_id: agent.id().to_string(),
            entries,
        });
    }
    Ok(traces)
}

/// Load traces from the gateway database (causal_events table) over RPC.
/// This is the preferred method as it provides queryable access to events.
pub fn load_traces_from_db(
    config_path: &Path,
    session_id: Option<&str>,
    agent_id: Option<&str>,
    limit: i64,
) -> anyhow::Result<Vec<CausalEventRecord>> {
    let config = autonoetic_gateway::config::load_config(config_path)?;
    let rpc = crate::cli::rpc::GatewayRpc::from_config(&config)?;
    let result = rpc.call(
        "trace.causal_search",
        serde_json::json!({
            "session_id": session_id,
            "agent_id": agent_id,
            "limit": limit,
        }),
    )?;
    serde_json::from_value(result).map_err(|e| anyhow::anyhow!("causal search decode failed: {}", e))
}

fn load_trace_user_interactions(
    config_path: &Path,
    session_id: &str,
) -> anyhow::Result<Vec<UserInteraction>> {
    let config = autonoetic_gateway::config::load_config(config_path)?;
    let rpc = crate::cli::rpc::GatewayRpc::from_config(&config)?;
    let result = rpc.call(
        "trace.user_interactions",
        serde_json::json!({ "session_id": session_id }),
    )?;
    serde_json::from_value(result)
        .map_err(|e| anyhow::anyhow!("user interactions decode failed: {}", e))
}

fn user_interaction_status_snake(
    status: &autonoetic_types::background::UserInteractionStatus,
) -> &'static str {
    use autonoetic_types::background::UserInteractionStatus;
    match status {
        UserInteractionStatus::Pending => "pending",
        UserInteractionStatus::Answered => "answered",
        UserInteractionStatus::Cancelled => "cancelled",
        UserInteractionStatus::Expired => "expired",
    }
}

fn print_user_interactions_trace_section(interactions: &[UserInteraction]) {
    if interactions.is_empty() {
        return;
    }

    println!();
    println!(
        "{}User interactions{} ({})",
        color::BOLD,
        color::RESET,
        interactions.len()
    );
    println!(
        "{}{}{:<14} {:<8} {:<12} {:<36} {:<20} {}",
        color::DIM,
        color::BOLD,
        "CREATED",
        "STATUS",
        "KIND",
        "INTERACTION_ID",
        "QUESTION",
        color::RESET
    );
    println!("{}", color::separator(120));

    for i in interactions {
        let ts: String = i.created_at.chars().take(19).collect();
        let q = if i.question.len() > 45 {
            format!("{}…", &i.question[..44])
        } else {
            i.question.clone()
        };
        println!(
            "{}{:<14} {:<8} {:<12} {:<36} {}{}",
            color::DIM,
            ts,
            user_interaction_status_snake(&i.status),
            i.kind.as_str(),
            i.interaction_id,
            color::RESET,
            q
        );
        if i.status == autonoetic_types::background::UserInteractionStatus::Answered {
            let ans = match (&i.answer_option_id, &i.answer_text) {
                (Some(id), _) => format!("option: {}", id),
                (_, Some(t)) => {
                    let t = if t.len() > 60 {
                        format!("{}…", &t[..59])
                    } else {
                        t.clone()
                    };
                    format!("text: {}", t)
                }
                _ => "answered".to_string(),
            };
            println!(
                "      {}└─ {}{}{}",
                color::DIM,
                color::GREEN,
                ans,
                color::RESET
            );
        }
    }
}

/// Handle trace session output from database events
fn handle_trace_session_from_db(
    session_id: &str,
    events: &[CausalEventRecord],
    user_interactions: &[UserInteraction],
    json_output: bool,
) -> anyhow::Result<()> {
    if json_output {
        let body = serde_json::json!({
            "session_id": session_id,
            "source": "gateway.db",
            "events": events,
            "user_interactions": user_interactions,
        });
        println!("{}", serde_json::to_string_pretty(&body)?);
        return Ok(());
    }

    println!(
        "Session: {}{}{}",
        color::BRIGHT_YELLOW,
        session_id,
        color::RESET
    );
    println!(
        "Source: {}{}{}",
        color::DIM,
        "gateway.db (causal_events + user_interactions)",
        color::RESET
    );
    println!();

    if events.is_empty() {
        println!(
            "{}No causal events for this session filter.{}",
            color::DIM,
            color::RESET
        );
        println!();
    } else {
        println!(
            "{}{}{:<8} {:<24} {:<15} {:<18} {:<15} {:<20} {}{}",
            color::DIM,
            color::BOLD,
            "SEQ",
            "TIMESTAMP",
            "CATEGORY",
            "ACTION",
            "STATUS",
            "TARGET",
            "REASON",
            color::RESET
        );
        println!("{}", color::separator(130));

        for event in events {
            let target_str = event.target.as_deref().unwrap_or("-");
            let reason_str = event.reason.as_deref().unwrap_or("-");
            let target_display = if target_str.len() > 19 {
                format!("{}…", &target_str[..18])
            } else {
                target_str.to_string()
            };
            let reason_display = if reason_str.len() > 35 {
                format!("{}…", &reason_str[..34])
            } else {
                reason_str.to_string()
            };

            let reason_colored = match event.status.as_str() {
                "ERROR" => format!(
                    "{}{}{}{}",
                    color::BRIGHT_RED,
                    color::BOLD,
                    reason_display,
                    color::RESET
                ),
                "DENIED" => format!(
                    "{}{}{}{}",
                    color::YELLOW,
                    color::BOLD,
                    reason_display,
                    color::RESET
                ),
                _ => color::dim(&reason_display),
            };

            println!(
                "{} {:<24} {} {} {} {} {}",
                color::seq(event.event_seq),
                event.timestamp,
                color::category(&event.category),
                color::action(&event.action),
                color::status_label(&event.status),
                color::dim(&target_display),
                reason_colored,
            );

            // Show tool-specific info for tool_invoke events
            if event.category == "tool_invoke" {
                if let Some(ref payload) = event.payload {
                    if let Ok(payload_json) = serde_json::from_str::<serde_json::Value>(payload) {
                        if let Some(tool_name) =
                            payload_json.get("tool_name").and_then(|v| v.as_str())
                        {
                            let args_preview = payload_json
                                .get("arguments")
                                .and_then(|v| v.as_str())
                                .map(|a| {
                                    if a.len() > 80 {
                                        format!("{}…", &a[..79])
                                    } else {
                                        a.to_string()
                                    }
                                })
                                .unwrap_or_default();
                            let result_preview = payload_json
                                .get("result_preview")
                                .and_then(|v| v.as_str())
                                .map(|r| {
                                    if r.len() > 80 {
                                        format!("{}…", &r[..79])
                                    } else {
                                        r.to_string()
                                    }
                                })
                                .unwrap_or_default();

                            if event.action == "requested" && !args_preview.is_empty() {
                                println!(
                                    "      {}├─ {}({}){}",
                                    color::DIM,
                                    color::tool_name(tool_name),
                                    color::dim(&args_preview),
                                    color::RESET
                                );
                            } else if event.action == "completed" && !result_preview.is_empty() {
                                println!(
                                    "      {}├─ {} → {}{}",
                                    color::DIM,
                                    color::tool_name(tool_name),
                                    color::dim(&result_preview),
                                    color::RESET
                                );
                            } else {
                                println!(
                                    "      {}├─ {}{}",
                                    color::DIM,
                                    color::tool_name(tool_name),
                                    color::RESET
                                );
                            }
                        }
                    }
                }
            } else if event.category == "egress" {
                // Egress chokepoint events (RFC §9.1/§9.3): render the
                // security-relevant detail readably — what was withheld, why,
                // and any assertion violations. Payloads are content-free
                // metadata by construction, so safe to display.
                if let Some(ref payload) = event.payload {
                    if let Ok(p) = serde_json::from_str::<serde_json::Value>(payload) {
                        match event.action.as_str() {
                            "egress.envelope_withheld" => {
                                let sink = p.get("target_sink").and_then(|v| v.as_str()).unwrap_or("?");
                                let ind = p.get("indication").and_then(|v| v.as_str()).unwrap_or("");
                                println!(
                                    "      {}├─ withheld from {}{}{} — {}{}",
                                    color::DIM,
                                    color::BRIGHT_YELLOW,
                                    sink,
                                    color::DIM,
                                    color::dim(ind),
                                    color::RESET
                                );
                            }
                            "egress.request_filtered" => {
                                let sink = p.get("target_sink").and_then(|v| v.as_str()).unwrap_or("?");
                                let wh = p.get("withheld_count").and_then(|v| v.as_u64()).unwrap_or(0);
                                let inc = p.get("included_count").and_then(|v| v.as_u64()).unwrap_or(0);
                                let vio = p.get("violation_count").and_then(|v| v.as_u64()).unwrap_or(0);
                                let vio_tag = if vio > 0 {
                                    format!(" {}{}⚠ {} violation(s){}", color::BRIGHT_RED, color::BOLD, vio, color::RESET)
                                } else {
                                    String::new()
                                };
                                println!(
                                    "      {}├─ sink={} withheld={} included={}{}{}",
                                    color::DIM, sink, wh, inc, vio_tag, color::RESET
                                );
                            }
                            "egress.assertion_violation" => {
                                let digest = p.get("payload_digest").and_then(|v| v.as_str()).unwrap_or("?");
                                println!(
                                    "      {}├─ {}verbatim echo of withheld payload (digest {}) — fail-closed{}",
                                    color::BRIGHT_RED, color::BOLD, digest, color::RESET
                                );
                            }
                            "egress.envelope_labeled" => {
                                let tool = p.get("tool_name").and_then(|v| v.as_str()).unwrap_or("?");
                                let res = p.get("resolution").and_then(|v| v.as_str()).unwrap_or("?");
                                println!(
                                    "      {}├─ labeled {} ({}){}",
                                    color::DIM, tool, res, color::RESET
                                );
                            }
                            _ => {
                                let s = serde_json::to_string(&p).unwrap_or_default();
                                println!("      {}├─ {}{}{}", color::DIM, color::BRIGHT_BLUE, s, color::RESET);
                            }
                        }
                    }
                }
            } else {
                if let Some(ref payload) = event.payload {
                    if let Ok(payload_json) = serde_json::from_str::<serde_json::Value>(payload) {
                        let payload_str = serde_json::to_string(&payload_json).unwrap_or_default();
                        if payload_str.len() > 2 && payload_str != "null" {
                            let truncated = if payload_str.len() > 120 {
                                format!("{}…", &payload_str[..119])
                            } else {
                                payload_str
                            };
                            println!(
                                "      {}├─ payload: {}{}{}",
                                color::DIM,
                                color::BRIGHT_BLUE,
                                truncated,
                                color::RESET
                            );
                        }
                    }
                }
            }
        }
    }

    print_user_interactions_trace_section(user_interactions);
    Ok(())
}

fn load_trace_from_path(path: &Path, agent_id: &str) -> anyhow::Result<AgentTrace> {
    let entries = read_trace_entries(path)?;
    Ok(AgentTrace {
        agent_id: agent_id.to_string(),
        entries,
    })
}

pub fn read_trace_entries(path: &Path) -> anyhow::Result<Vec<CausalChainEntry>> {
    let file = std::fs::File::open(path)?;
    let reader = StdBufReader::new(file);
    let mut entries = Vec::new();
    for (idx, line) in reader.lines().enumerate() {
        let line = line?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let entry: CausalChainEntry = serde_json::from_str(trimmed).map_err(|e| {
            anyhow::anyhow!(
                "Invalid JSON in {} at line {}: {}",
                path.display(),
                idx + 1,
                e
            )
        })?;
        validate_trace_entry(&entry, path, idx + 1)?;
        entries.push(entry);
    }
    Ok(entries)
}

pub fn validate_trace_entry(
    entry: &CausalChainEntry,
    path: &Path,
    line_no: usize,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        !entry.session_id.trim().is_empty(),
        "Invalid causal entry in {} at line {}: missing top-level session_id",
        path.display(),
        line_no
    );
    anyhow::ensure!(
        !entry.entry_hash.trim().is_empty(),
        "Invalid causal entry in {} at line {}: missing top-level entry_hash",
        path.display(),
        line_no
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_read_trace_entries_rejects_missing_top_level_session_fields() {
        let temp = tempdir().expect("tempdir should create");
        let path = temp.path().join("causal_chain.jsonl");
        std::fs::write(
            &path,
            r#"{"timestamp":"2026-03-06T00:00:00Z","log_id":"l1","actor_id":"a1","category":"lifecycle","action":"wake","target":null,"status":"SUCCESS","reason":null,"payload":{"session_id":"legacy"},"prev_hash":"genesis","entry_hash":"abc"}"#,
        )
        .expect("trace should write");

        let err = read_trace_entries(&path).expect_err("legacy missing session_id should fail");
        assert!(
            err.to_string().contains("missing top-level session_id"),
            "expected strict top-level session_id validation"
        );
    }

    #[test]
    fn test_read_trace_entries_rejects_missing_top_level_hash_fields() {
        let temp = tempdir().expect("tempdir should create");
        let path = temp.path().join("causal_chain.jsonl");
        std::fs::write(
            &path,
            r#"{"timestamp":"2026-03-06T00:00:00Z","log_id":"l1","actor_id":"a1","session_id":"s1","turn_id":"turn-000001","event_seq":1,"category":"lifecycle","action":"wake","target":null,"status":"SUCCESS","reason":null,"payload":{"history_messages":2},"prev_hash":"genesis","entry_hash":""}"#,
        )
        .expect("trace should write");

        let err = read_trace_entries(&path).expect_err("missing entry_hash should fail");
        assert!(
            err.to_string().contains("missing top-level entry_hash"),
            "expected strict top-level entry_hash validation"
        );
    }

    #[tokio::test]
    async fn test_trace_session_ordering_by_timestamp() {
        let temp = tempdir().expect("tempdir should create");
        let agent_dir = temp.path().join("agent_test");
        let history_dir = agent_dir.join("history");
        std::fs::create_dir_all(&history_dir).expect("history dir should create");

        let causal_path = history_dir.join("causal_chain.jsonl");

        let entry1 = r#"{"timestamp":"2026-03-08T00:00:03Z","log_id":"l1","actor_id":"a1","session_id":"s1","turn_id":null,"event_seq":3,"category":"gateway","action":"test.3","target":null,"status":"SUCCESS","reason":null,"payload":null,"payload_hash":null,"prev_hash":"genesis","entry_hash":"h1"}"#;
        let entry2 = r#"{"timestamp":"2026-03-08T00:00:01Z","log_id":"l2","actor_id":"a1","session_id":"s1","turn_id":null,"event_seq":1,"category":"gateway","action":"test.1","target":null,"status":"SUCCESS","reason":null,"payload":null,"payload_hash":null,"prev_hash":"genesis","entry_hash":"h2"}"#;
        let entry3 = r#"{"timestamp":"2026-03-08T00:00:02Z","log_id":"l3","actor_id":"a1","session_id":"s1","turn_id":null,"event_seq":2,"category":"gateway","action":"test.2","target":null,"status":"SUCCESS","reason":null,"payload":null,"payload_hash":null,"prev_hash":"genesis","entry_hash":"h3"}"#;

        std::fs::write(
            &causal_path,
            format!("{}\n{}\n{}\n", entry1, entry2, entry3),
        )
        .expect("should write");

        let traces = vec![AgentTrace {
            agent_id: "agent_test".to_string(),
            entries: read_trace_entries(&causal_path).expect("should read entries"),
        }];

        let entries = &traces[0].entries;
        assert_eq!(entries.len(), 3);

        let first_read_timestamp = &entries[0].timestamp;
        assert_eq!(
            first_read_timestamp, "2026-03-08T00:00:03Z",
            "First entry should be from file order (00:00:03), not sorted"
        );

        let mut sorted_entries = entries.clone();
        sorted_entries.sort_by(|a, b| {
            a.timestamp
                .cmp(&b.timestamp)
                .then_with(|| a.event_seq.cmp(&b.event_seq))
        });

        let expected_order = vec![
            "2026-03-08T00:00:01Z",
            "2026-03-08T00:00:02Z",
            "2026-03-08T00:00:03Z",
        ];
        let actual_order: Vec<&str> = sorted_entries
            .iter()
            .map(|e| e.timestamp.as_str())
            .collect();
        assert_eq!(
            actual_order, expected_order,
            "Entries should be sorted by timestamp"
        );

        let actions: Vec<&str> = sorted_entries.iter().map(|e| e.action.as_str()).collect();
        assert_eq!(actions, vec!["test.1", "test.2", "test.3"]);
    }
}

pub fn handle_trace_rebuild(
    config_path: &std::path::Path,
    session_id: &str,
    requested_agent: Option<&str>,
    json_output: bool,
    skip_checks: bool,
) -> anyhow::Result<()> {
    let config = autonoetic_gateway::config::load_config(config_path)?;
    let gateway_dir = autonoetic_gateway::execution::gateway_root_dir(&config);

    let mut all_events: Vec<super::common::TraceEntry> = Vec::new();

    // Load gateway events
    let gateway_causal_path = gateway_dir.join("history/causal_chain.jsonl");
    if gateway_causal_path.exists() {
        let gateway_traces = load_trace_from_path(&gateway_causal_path, "gateway")?;
        for entry in gateway_traces.entries {
            if entry.session_id == session_id {
                all_events.push(super::common::TraceEntry {
                    agent_id: "gateway".to_string(),
                    entry,
                });
            }
        }
    }

    // Load agent events
    let agents_dir = &config.agents_dir;
    if let Ok(entries) = std::fs::read_dir(agents_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                let causal_path = path.join("history/causal_chain.jsonl");
                if causal_path.exists() {
                    let agent_id = path
                        .file_name()
                        .map(|s| s.to_string_lossy().to_string())
                        .unwrap_or_default();

                    if let Some(requested) = requested_agent {
                        if agent_id != requested {
                            continue;
                        }
                    }

                    let traces = load_trace_from_path(&causal_path, &agent_id)?;
                    for entry in traces.entries {
                        if entry.session_id == session_id {
                            all_events.push(super::common::TraceEntry {
                                agent_id: agent_id.clone(),
                                entry,
                            });
                        }
                    }
                }
            }
        }
    }

    if all_events.is_empty() {
        anyhow::bail!("No events found for session '{}'", session_id);
    }

    // Sort by timestamp then event_seq
    all_events.sort_by(|a, b| {
        a.entry
            .timestamp
            .cmp(&b.entry.timestamp)
            .then_with(|| a.entry.event_seq.cmp(&b.entry.event_seq))
    });

    // Run integrity checks if not skipped
    let mut integrity_issues: Vec<String> = Vec::new();
    if !skip_checks {
        // Check for gaps in event_seq per agent
        let mut agent_seqs: std::collections::HashMap<String, u64> =
            std::collections::HashMap::new();
        for te in &all_events {
            let prev = agent_seqs.get(&te.agent_id).copied().unwrap_or(0);
            if te.entry.event_seq != prev + 1 && te.entry.event_seq != 1 {
                integrity_issues.push(format!(
                    "Agent '{}': event_seq gap at {} (expected {}, got {})",
                    te.agent_id,
                    te.entry.timestamp,
                    prev + 1,
                    te.entry.event_seq
                ));
            }
            agent_seqs.insert(te.agent_id.clone(), te.entry.event_seq);
        }
    }

    if json_output {
        let output = serde_json::json!({
            "session_id": session_id,
            "event_count": all_events.len(),
            "integrity_issues": integrity_issues,
            "events": all_events.iter().map(|te| {
                serde_json::json!({
                    "agent_id": te.agent_id,
                    "timestamp": te.entry.timestamp,
                    "action": te.entry.action,
                    "status": te.entry.status,
                    "event_seq": te.entry.event_seq,
                })
            }).collect::<Vec<_>>()
        });
        println!("{}", serde_json::to_string_pretty(&output)?);
    } else {
        println!(
            "{}Session Reconstruction: {}{}",
            color::BOLD,
            session_id,
            color::RESET
        );
        println!(
            "Total events: {}{}{}{}",
            color::BRIGHT_YELLOW,
            color::BOLD,
            all_events.len(),
            color::RESET
        );
        println!();

        if !integrity_issues.is_empty() {
            println!(
                "{}{}⚠ Integrity Issues:{}{}",
                color::BRIGHT_RED,
                color::BOLD,
                color::RESET,
                color::RESET
            );
            for issue in &integrity_issues {
                println!("  {}{}{}{}", color::RED, issue, color::RESET, color::RESET);
            }
            println!();
        }

        println!(
            "{}{}{:<10} {:<30} {:<30} {:<20} {:<15}{}",
            color::DIM,
            color::BOLD,
            "SEQ",
            "TIMESTAMP",
            "AGENT",
            "ACTION",
            "STATUS",
            color::RESET
        );
        println!("{}", color::separator(105));

        for te in &all_events {
            println!(
                "{} {:<30} {} {} {}",
                color::seq(te.entry.event_seq),
                te.entry.timestamp,
                color::agent(&te.agent_id),
                color::action(&te.entry.action),
                color::status_label(&format!("{:?}", te.entry.status)),
            );
        }
    }

    Ok(())
}

pub async fn handle_trace_follow(
    config_path: &std::path::Path,
    session_id: &str,
    requested_agent: Option<&str>,
    json_output: bool,
) -> anyhow::Result<()> {
    use std::collections::HashSet;
    use tokio::time::{interval, Duration};

    let config = autonoetic_gateway::config::load_config(config_path)?;
    let gateway_dir = autonoetic_gateway::execution::gateway_root_dir(&config);
    let agents_dir = &config.agents_dir;

    let mut seen_log_ids: HashSet<String> = HashSet::new();
    let mut poll_interval = interval(Duration::from_secs(1));

    println!(
        "{}Following session '{}'.{} Press Ctrl+C to stop.",
        color::BOLD,
        session_id,
        color::RESET
    );
    println!();
    if !json_output {
        println!(
            "{}{}{:<8} {:<24} {:<22} {:<15} {:<18} {:<15} {:<20} {}{}",
            color::DIM,
            color::BOLD,
            "SEQ",
            "TIMESTAMP",
            "AGENT",
            "CATEGORY",
            "ACTION",
            "STATUS",
            "TARGET",
            "REASON",
            color::RESET
        );
        println!("{}", color::separator(160));
    }

    loop {
        poll_interval.tick().await;

        let mut new_events: Vec<super::common::TraceEntry> = Vec::new();

        // Check gateway causal log
        let gateway_causal_path = gateway_dir.join("history/causal_chain.jsonl");
        if gateway_causal_path.exists() {
            if let Ok(traces) = load_trace_from_path(&gateway_causal_path, "gateway") {
                for entry in traces.entries {
                    if entry.session_id == session_id {
                        let log_id = format!("gateway:{}", entry.log_id);
                        if !seen_log_ids.contains(&log_id) {
                            seen_log_ids.insert(log_id);
                            new_events.push(super::common::TraceEntry {
                                agent_id: "gateway".to_string(),
                                entry,
                            });
                        }
                    }
                }
            }
        }

        // Check agent causal logs
        if let Ok(entries) = std::fs::read_dir(agents_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    let causal_path = path.join("history/causal_chain.jsonl");
                    if causal_path.exists() {
                        let agent_id = path
                            .file_name()
                            .map(|s| s.to_string_lossy().to_string())
                            .unwrap_or_default();

                        if let Some(requested) = requested_agent {
                            if agent_id != requested {
                                continue;
                            }
                        }

                        if let Ok(traces) = load_trace_from_path(&causal_path, &agent_id) {
                            for entry in traces.entries {
                                if entry.session_id == session_id {
                                    let log_id = format!("{}:{}", agent_id, entry.log_id);
                                    if !seen_log_ids.contains(&log_id) {
                                        seen_log_ids.insert(log_id);
                                        new_events.push(super::common::TraceEntry {
                                            agent_id: agent_id.clone(),
                                            entry,
                                        });
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        if !new_events.is_empty() {
            new_events.sort_by(|a, b| {
                a.entry
                    .timestamp
                    .cmp(&b.entry.timestamp)
                    .then_with(|| a.entry.event_seq.cmp(&b.entry.event_seq))
            });

            for te in new_events {
                if json_output {
                    println!(
                        "{}",
                        serde_json::to_string(&serde_json::json!({
                            "agent_id": te.agent_id,
                            "timestamp": te.entry.timestamp,
                            "category": te.entry.category,
                            "action": te.entry.action,
                            "status": te.entry.status,
                            "event_seq": te.entry.event_seq,
                            "turn_id": te.entry.turn_id,
                            "target": te.entry.target,
                            "reason": te.entry.reason,
                            "payload": te.entry.payload,
                        }))?
                    );
                } else {
                    let target_str = te.entry.target.as_deref().unwrap_or("-");
                    let reason_str = te.entry.reason.as_deref().unwrap_or("-");
                    let _target_display = if target_str.len() > 19 {
                        format!("{}…", &target_str[..18])
                    } else {
                        target_str.to_string()
                    };
                    let reason_display = if reason_str.len() > 35 {
                        format!("{}…", &reason_str[..34])
                    } else {
                        reason_str.to_string()
                    };

                    // Highlight reason in red for errors/denials, dim otherwise
                    let reason_colored = match &te.entry.status {
                        EntryStatus::Error => format!(
                            "{}{}{}{}",
                            color::BRIGHT_RED,
                            color::BOLD,
                            reason_display,
                            color::RESET
                        ),
                        EntryStatus::Denied => format!(
                            "{}{}{}{}",
                            color::YELLOW,
                            color::BOLD,
                            reason_display,
                            color::RESET
                        ),
                        _ => color::dim(&reason_display),
                    };

                    println!(
                        "{} {:<24} {} {} {} {} {}",
                        color::seq(te.entry.event_seq),
                        te.entry.timestamp,
                        color::agent(&te.agent_id),
                        color::category(&te.entry.category),
                        color::action(&te.entry.action),
                        color::status_label(&format!("{:?}", te.entry.status)),
                        reason_colored,
                    );

                    // Show tool-specific info for tool_invoke events
                    if te.entry.category == "tool_invoke" {
                        if let Some(ref payload) = te.entry.payload {
                            if let Some(tool_name) =
                                payload.get("tool_name").and_then(|v| v.as_str())
                            {
                                let args_preview = payload
                                    .get("arguments")
                                    .and_then(|v| v.as_str())
                                    .map(|a| {
                                        if a.len() > 80 {
                                            format!("{}…", &a[..79])
                                        } else {
                                            a.to_string()
                                        }
                                    })
                                    .unwrap_or_default();
                                let result_preview = payload
                                    .get("result_preview")
                                    .and_then(|v| v.as_str())
                                    .map(|r| {
                                        if r.len() > 80 {
                                            format!("{}…", &r[..79])
                                        } else {
                                            r.to_string()
                                        }
                                    })
                                    .unwrap_or_default();

                                if te.entry.action == "requested" && !args_preview.is_empty() {
                                    println!(
                                        "      {}├─ {}({}){}",
                                        color::DIM,
                                        color::tool_name(tool_name),
                                        color::dim(&args_preview),
                                        color::RESET
                                    );
                                } else if te.entry.action == "completed"
                                    && !result_preview.is_empty()
                                {
                                    println!(
                                        "      {}├─ {} → {}{}",
                                        color::DIM,
                                        color::tool_name(tool_name),
                                        color::dim(&result_preview),
                                        color::RESET
                                    );
                                } else {
                                    println!(
                                        "      {}├─ {}{}",
                                        color::DIM,
                                        color::tool_name(tool_name),
                                        color::RESET
                                    );
                                }
                            }
                        }
                    } else {
                        // Generic payload for non-tool events
                        if let Some(ref payload) = te.entry.payload {
                            let payload_str = serde_json::to_string(payload).unwrap_or_default();
                            if payload_str.len() > 2 && payload_str != "null" {
                                let truncated = if payload_str.len() > 120 {
                                    format!("{}…", &payload_str[..119])
                                } else {
                                    payload_str
                                };
                                println!(
                                    "      {}├─ payload: {}{}{}",
                                    color::DIM,
                                    color::BRIGHT_BLUE,
                                    truncated,
                                    color::RESET
                                );
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Handle `autonoetic trace fork` command.
pub async fn handle_trace_fork(
    config_path: &Path,
    source_session_id: &str,
    branch_message: Option<&str>,
    new_session_id: Option<&str>,
    at_turn: Option<usize>,
    agent_id: Option<&str>,
    interactive: bool,
    json_output: bool,
) -> anyhow::Result<()> {
    let config = autonoetic_gateway::config::load_config(config_path)?;
    let rpc = crate::cli::rpc::GatewayRpc::from_config(&config)?;

    // The `session.fork` RPC is the single choke point for forking: it
    // performs the checkpoint fork server-side AND records every side effect
    // (timeline mirror, lineage row, both causal events) — a CLI fork is
    // therefore indistinguishable from an RPC fork (#814, #1119 tranche 6).
    let fork = rpc.call(
        "session.fork",
        serde_json::json!({
            "source_session_id": source_session_id,
            "branch_message": branch_message,
            "new_session_id": new_session_id,
            "at_turn": at_turn.map(|t| t as u64),
            "target_agent_id": agent_id,
        }),
    )?;
    let new_session_id = fork["new_session_id"].as_str().unwrap_or("?").to_string();
    let source_session_id = fork["source_session_id"].as_str().unwrap_or("?").to_string();
    let fork_turn = fork["fork_turn"].as_u64();
    let history_handle = fork["history_handle"].as_str().unwrap_or("?").to_string();
    let message_count = fork["message_count"].as_u64().unwrap_or(0);
    // mirrored_events is logged server-side; surface a warning if the
    // lineage mirror failed (the fork itself still succeeded on disk).
    if fork["mirrored_events"].as_u64().unwrap_or(0) == 0 {
        eprintln!(
            "warning: fork lineage mirror reported 0 events — `trace fork-tree` may not show this fork"
        );
    }

    if !json_output {
        println!("Session forked successfully!");
        println!("  Source session:    {}", source_session_id);
        println!("  New session:       {}", new_session_id);
        println!("  Fork turn:         {}", fork_turn.map(|t| t.to_string()).unwrap_or_default());
        if let Some(turn) = at_turn {
            println!("  Forked at turn:    {}", turn);
        }
        println!("  History messages:  {}", message_count as usize);
        println!("  History handle:    {}", history_handle);
        if let Some(msg) = branch_message {
            println!("  Branch message:    {}", msg);
        }
    }

    // If interactive mode, start a chat session with the forked session
    if interactive {
        if json_output {
            // In JSON mode, output the fork info first
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "new_session_id": new_session_id,
                    "source_session_id": source_session_id,
                    "fork_turn": fork_turn,
                    "history_handle": history_handle,
                    "message_count": message_count as usize,
                    "at_turn": at_turn,
                }))?
            );
        }

        println!();
        println!("Starting interactive session with forked session...");
        println!("Type /exit to quit.");
        println!();

        // Use the existing chat functionality to continue the session
        let chat_args = super::common::ChatArgs {
            agent_id: agent_id.map(|a| a.to_string()),
            session_id: Some(new_session_id.clone()),
            sender_id: None,
            channel_id: None,
            resume: false,
            test_mode: false,
        };
        super::chat::handle_chat(config_path, &chat_args).await?;
    } else if json_output {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "new_session_id": new_session_id,
                "source_session_id": source_session_id,
                "fork_turn": fork_turn,
                "history_handle": history_handle,
                "message_count": message_count as usize,
                "at_turn": at_turn,
            }))?
        );
    }

    Ok(())
}

/// Render the descendant tree from the `trace.fork_tree` RPC payload
/// (records are JSON with `children` arrays — the server owns the walk).
fn print_fork_tree_json(nodes: &[serde_json::Value], depth: usize) {
    for n in nodes {
        let turn = n["fork_turn"]
            .as_u64()
            .map(|t| format!(" @turn {t}"))
            .unwrap_or_default();
        println!(
            "  {}{}{}{}{}  (created {})",
            "  ".repeat(depth),
            color::DIM,
            n["forked_session_id"].as_str().unwrap_or("?"),
            color::RESET,
            turn,
            n["created_at"].as_str().unwrap_or("?")
        );
        let children = n["children"].as_array().cloned().unwrap_or_default();
        print_fork_tree_json(&children, depth + 1);
    }
}

/// Handle `autonoetic trace fork-tree` command: show a session's ancestor
/// chain (if it was itself a fork) and the tree of sessions forked FROM it.
pub fn handle_trace_fork_tree(
    config_path: &Path,
    session_id: &str,
    json_output: bool,
) -> anyhow::Result<()> {
    let config = autonoetic_gateway::config::load_config(config_path)?;
    let rpc = crate::cli::rpc::GatewayRpc::from_config(&config)?;
    // Ancestor chain + descendant tree are computed server-side (lineage
    // table walk with the same depth/visited guards) — the CLI renders
    // (#1119 tranche 6).
    let body = rpc.call(
        "trace.fork_tree",
        serde_json::json!({ "session_id": session_id }),
    )?;
    let root_id = body["root_session_id"].as_str().unwrap_or(session_id).to_string();
    let ancestors = body["ancestors"].as_array().cloned().unwrap_or_default();
    let descendants = body["descendants"].as_array().cloned().unwrap_or_default();

    if json_output {
        println!("{}", serde_json::to_string_pretty(&body)?);
        return Ok(());
    }

    println!(
        "{}Fork lineage{} for {}",
        color::BOLD,
        color::RESET,
        color::agent(&root_id)
    );
    println!();

    if ancestors.is_empty() {
        println!("  (root session — not itself a fork)");
    } else {
        // Print oldest-first: the topmost ancestor, then each descendant of
        // it down to `root_id` (the target, which is `ancestors[0]`'s
        // forked_session_id).
        let apex = ancestors
            .last()
            .and_then(|a| a["source_session_id"].as_str())
            .unwrap_or("?");
        println!("  {}", apex);
        for a in ancestors.iter().rev() {
            let turn = a["fork_turn"]
                .as_u64()
                .map(|t| format!(" @turn {t}"))
                .unwrap_or_default();
            println!(
                "    -> {}{}{}  (created {})",
                color::agent(a["forked_session_id"].as_str().unwrap_or("?")),
                turn,
                if a["branch_message_sha256"].is_string() {
                    " (branch message)"
                } else {
                    ""
                },
                a["created_at"].as_str().unwrap_or("?")
            );
        }
    }

    println!();
    if descendants.is_empty() {
        println!("  No sessions have been forked from this one.");
    } else {
        println!("  Descendants:");
        print_fork_tree_json(&descendants, 0);
    }

    Ok(())
}

/// Handle `autonoetic trace history` command.
pub fn handle_trace_history(
    config_path: &Path,
    session_id: &str,
    _requested_agent: Option<&str>,
    json_output: bool,
) -> anyhow::Result<()> {
    let config = autonoetic_gateway::config::load_config(config_path)?;
    let gateway_dir = autonoetic_gateway::execution::gateway_root_dir(&config);
    let store = autonoetic_gateway::runtime::content_store::ContentStore::new(&gateway_dir)?;

    // Try to load history from session
    let history = store.read_by_name(session_id, "session_history");

    match history {
        Ok(content) => {
            let messages: Vec<Message> = serde_json::from_slice(&content)?;

            if json_output {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "session_id": session_id,
                        "message_count": messages.len(),
                        "messages": messages.iter().map(|m| serde_json::json!({
                            "role": format!("{:?}", m.role),
                            "content": m.content,
                        })).collect::<Vec<_>>(),
                    }))?
                );
            } else {
                println!("Session history: {} messages", messages.len());
                println!();
                for (i, msg) in messages.iter().enumerate() {
                    let role = match msg.role {
                        autonoetic_gateway::llm::Role::System => "system",
                        autonoetic_gateway::llm::Role::User => "user",
                        autonoetic_gateway::llm::Role::Assistant => "assistant",
                        autonoetic_gateway::llm::Role::Tool => "tool",
                    };
                    println!("[{}] {}: {}", i + 1, role, msg.content);
                }
            }
        }
        Err(_) => {
            if json_output {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "session_id": session_id,
                        "error": "History not found",
                        "message_count": 0,
                        "messages": [],
                    }))?
                );
            } else {
                println!("No history found for session '{}'.", session_id);
                println!("The session may not have been snapshotted yet.");
            }
        }
    }

    Ok(())
}

fn shorten_json_value(payload: &serde_json::Value, max_chars: usize) -> String {
    match payload {
        serde_json::Value::Null => String::new(),
        serde_json::Value::Object(o) if o.is_empty() => String::new(),
        _ => {
            let s = payload.to_string();
            let count = s.chars().count();
            if count <= max_chars {
                s
            } else {
                let take: String = s.chars().take(max_chars).collect();
                format!("{take}…")
            }
        }
    }
}

fn print_workflow_event_row(ev: &WorkflowEventRecord, json_output: bool) -> anyhow::Result<()> {
    if json_output {
        println!("{}", serde_json::to_string(ev)?);
        return Ok(());
    }
    let task = ev.task_id.as_deref().unwrap_or("-");
    let date_short: String = ev.occurred_at.chars().take(10).collect();
    println!(
        "{}{:<10} {:<28} {:<36} {:<18} {}",
        color::DIM,
        date_short,
        ev.event_type,
        ev.event_id,
        task,
        color::RESET
    );
    let p = shorten_json_value(&ev.payload, 56);
    if !p.is_empty() {
        println!("  {}payload:{} {}", color::DIM, color::RESET, p);
    }
    Ok(())
}

fn print_workflow_events_table(
    workflow_id: &str,
    run: Option<&WorkflowRun>,
    events: &[WorkflowEventRecord],
    user_interactions: &[UserInteraction],
    json_output: bool,
) -> anyhow::Result<()> {
    if json_output {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "workflow_id": workflow_id,
                "workflow": run,
                "events": events,
                "user_interactions": user_interactions,
            }))?
        );
        return Ok(());
    }

    println!(
        "{}Workflow{} {}  ({} events)",
        color::BOLD,
        color::RESET,
        workflow_id,
        events.len()
    );
    if let Some(r) = run {
        println!(
            "{}  root_session: {}  status: {:?}{}",
            color::DIM,
            r.root_session_id,
            r.status,
            color::RESET
        );
    }
    println!();

    if events.is_empty() {
        println!(
            "{}No workflow events in gateway store.{}",
            color::DIM,
            color::RESET
        );
        return Ok(());
    }

    println!(
        "{}{}{:<10} {:<28} {:<36} {:<18} {}",
        color::DIM,
        color::BOLD,
        "DATE",
        "TYPE",
        "EVENT_ID",
        "TASK",
        color::RESET
    );
    println!("{}", color::separator(120));
    for ev in events {
        print_workflow_event_row(ev, false)?;
    }
    print_user_interactions_trace_section(user_interactions);
    Ok(())
}

/// Print durable workflow store events (SQLite), optionally following new events.
pub async fn handle_trace_workflow(
    config_path: &Path,
    workflow_or_root: &str,
    as_root: bool,
    json_output: bool,
    follow: bool,
) -> anyhow::Result<()> {
    let config = autonoetic_gateway::config::load_config(config_path)?;
    let workflow_id = if as_root {
        match autonoetic_gateway::scheduler::resolve_workflow_id_for_root_session(
            &config,
            workflow_or_root,
        )? {
            Some(w) => w,
            None => anyhow::bail!(
                "No workflow index for root session '{}'. (Has `agent.spawn` run for this root?)",
                workflow_or_root
            ),
        }
    } else {
        workflow_or_root.to_string()
    };

    let run = autonoetic_gateway::scheduler::load_workflow_run(&config, None, &workflow_id)?;
    if !follow && run.is_none() {
        anyhow::bail!(
            "No workflow run '{}' in gateway scheduler store.",
            workflow_id
        );
    }

    if follow {
        trace_workflow_follow(&config, &workflow_id, run.as_ref(), json_output).await
    } else {
        let events =
            autonoetic_gateway::scheduler::load_workflow_events(&config, None, &workflow_id)?;
        let rpc = crate::cli::rpc::GatewayRpc::from_config(&config)?;
        let wf_interactions: Vec<UserInteraction> = serde_json::from_value(rpc.call(
            "trace.user_interactions",
            serde_json::json!({ "workflow_id": workflow_id }),
        )?)
        .map_err(|e| anyhow::anyhow!("user interactions decode failed: {}", e))?;
        print_workflow_events_table(
            &workflow_id,
            run.as_ref(),
            &events,
            &wf_interactions,
            json_output,
        )?;
        Ok(())
    }
}

async fn trace_workflow_follow(
    config: &autonoetic_gateway::GatewayConfig,
    workflow_id: &str,
    run: Option<&WorkflowRun>,
    json_output: bool,
) -> anyhow::Result<()> {
    use std::collections::HashSet;
    use tokio::time::{interval, Duration};

    let mut seen: HashSet<String> = HashSet::new();
    let mut seen_interactions: HashSet<String> = HashSet::new();
    let rpc = crate::cli::rpc::GatewayRpc::from_config(config)?;
    let mut poll_interval = interval(Duration::from_secs(1));

    println!(
        "{}Following workflow '{}'.{} Press Ctrl+C to stop.",
        color::BOLD,
        workflow_id,
        color::RESET
    );
    if let Some(r) = run {
        println!(
            "{}  root_session: {}  status: {:?}{}",
            color::DIM,
            r.root_session_id,
            r.status,
            color::RESET
        );
    }
    println!();

    if !json_output {
        println!(
            "{}{}{:<10} {:<28} {:<36} {:<18} {}",
            color::DIM,
            color::BOLD,
            "DATE",
            "TYPE",
            "EVENT_ID",
            "TASK",
            color::RESET
        );
        println!("{}", color::separator(120));
    }

    loop {
        poll_interval.tick().await;
        let events =
            autonoetic_gateway::scheduler::load_workflow_events(config, None, workflow_id)?;
        for ev in events {
            if seen.insert(ev.event_id.clone()) {
                print_workflow_event_row(&ev, json_output)?;
            }
        }

        let interactions: Vec<UserInteraction> = match rpc.call(
            "trace.user_interactions",
            serde_json::json!({ "workflow_id": workflow_id }),
        ) {
            Ok(raw) => match serde_json::from_value(raw) {
                Ok(parsed) => parsed,
                Err(e) => {
                    // A decode failure means the RPC contract changed or the
                    // server misbehaved — surface it rather than silently
                    // showing "no new interactions".
                    eprintln!("  [warn] trace.user_interactions decode failed: {e}");
                    Vec::new()
                }
            },
            Err(e) => {
                eprintln!("  [warn] trace.user_interactions failed: {e}");
                Vec::new()
            }
        };
        let mut new_interactions: Vec<UserInteraction> = Vec::new();
        for interaction in interactions {
            if seen_interactions.insert(interaction.interaction_id.clone()) {
                new_interactions.push(interaction);
            }
        }
        if !new_interactions.is_empty() {
            if json_output {
                for interaction in new_interactions {
                    println!(
                        "{}",
                        serde_json::to_string(&serde_json::json!({
                            "type": "user_interaction",
                            "data": interaction,
                        }))?
                    );
                }
            } else {
                print_user_interactions_trace_section(&new_interactions);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// trace graph (workflow store projection, Phase 7)
// ---------------------------------------------------------------------------

fn workflow_status_snake(s: WorkflowRunStatus) -> &'static str {
    match s {
        WorkflowRunStatus::Active => "active",
        WorkflowRunStatus::WaitingChildren => "waiting_children",
        WorkflowRunStatus::BlockedApproval => "blocked_approval",
        WorkflowRunStatus::Resumable => "resumable",
        WorkflowRunStatus::EmergencyStopping => "emergency_stopping",
        WorkflowRunStatus::EmergencyStopped => "emergency_stopped",
        WorkflowRunStatus::Completed => "completed",
        WorkflowRunStatus::Failed => "failed",
        WorkflowRunStatus::Cancelled => "cancelled",
    }
}

fn task_status_snake(s: TaskRunStatus) -> &'static str {
    match s {
        TaskRunStatus::Pending => "pending",
        TaskRunStatus::Runnable => "runnable",
        TaskRunStatus::Running => "running",
        TaskRunStatus::AwaitingApproval => "awaiting_approval",
        TaskRunStatus::Stale => "stale",
        TaskRunStatus::Paused => "paused",
        TaskRunStatus::Aborting => "aborting",
        TaskRunStatus::Aborted => "aborted",
        TaskRunStatus::Succeeded => "succeeded",
        TaskRunStatus::Failed => "failed",
        TaskRunStatus::Cancelled => "cancelled",
    }
}

fn resolve_workflow_id_for_graph(
    config: &autonoetic_gateway::GatewayConfig,
    session_or_wf: &str,
) -> anyhow::Result<String> {
    let s = session_or_wf.trim();
    if s.starts_with("wf-") {
        if autonoetic_gateway::scheduler::load_workflow_run(config, None, s)?.is_none() {
            anyhow::bail!("No workflow run '{}' in gateway scheduler store.", s);
        }
        return Ok(s.to_string());
    }
    match autonoetic_gateway::scheduler::resolve_workflow_id_for_root_session(config, s)? {
        Some(w) => Ok(w),
        None => anyhow::bail!(
            "No workflow for root session '{}'. Pass the root session used with `agent.spawn`, or a `wf-…` id from `trace workflow`.",
            s
        ),
    }
}

#[derive(Debug, Serialize)]
struct WorkflowGraphTaskView {
    task_id: String,
    agent_id: String,
    session_id: String,
    parent_session_id: String,
    status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    result_summary: Option<String>,
}

#[derive(Debug, Serialize)]
struct WorkflowGraphEventView {
    event_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    task_id: Option<String>,
    occurred_at: String,
}

#[derive(Debug, Serialize)]
struct WorkflowGraphView {
    workflow_id: String,
    root_session_id: String,
    workflow_status: String,
    lead_agent_id: String,
    active_task_ids: Vec<String>,
    tasks: Vec<WorkflowGraphTaskView>,
    recent_events: Vec<WorkflowGraphEventView>,
}

fn build_workflow_graph_view(
    config: &autonoetic_gateway::GatewayConfig,
    run: &WorkflowRun,
) -> anyhow::Result<WorkflowGraphView> {
    let tasks =
        autonoetic_gateway::scheduler::list_task_runs_for_workflow(config, None, &run.workflow_id)?;
    let events =
        autonoetic_gateway::scheduler::load_workflow_events(config, None, &run.workflow_id)?;
    let start = events.len().saturating_sub(12);
    let recent_slice = &events[start..];

    let task_views: Vec<WorkflowGraphTaskView> = tasks
        .into_iter()
        .map(|t: TaskRun| WorkflowGraphTaskView {
            task_id: t.task_id,
            agent_id: t.agent_id,
            session_id: t.session_id,
            parent_session_id: t.parent_session_id,
            status: task_status_snake(t.status).to_string(),
            result_summary: t.result_summary,
        })
        .collect();

    let event_views: Vec<WorkflowGraphEventView> = recent_slice
        .iter()
        .map(|e| WorkflowGraphEventView {
            event_type: e.event_type.clone(),
            task_id: e.task_id.clone(),
            occurred_at: e.occurred_at.clone(),
        })
        .collect();

    Ok(WorkflowGraphView {
        workflow_id: run.workflow_id.clone(),
        root_session_id: run.root_session_id.clone(),
        workflow_status: workflow_status_snake(run.status).to_string(),
        lead_agent_id: run.lead_agent_id.clone(),
        active_task_ids: run.active_task_ids.clone(),
        tasks: task_views,
        recent_events: event_views,
    })
}

fn print_workflow_graph_text(view: &WorkflowGraphView) {
    println!(
        "{}workflow{} {}  {}wf={}{}  [{}]",
        color::BOLD,
        color::RESET,
        view.root_session_id,
        color::DIM,
        view.workflow_id,
        color::RESET,
        color::status_label(&view.workflow_status)
    );
    let lead = if view.lead_agent_id.is_empty() {
        format!("{}(unknown){}", color::DIM, color::RESET)
    } else {
        color::agent(&view.lead_agent_id)
    };
    println!(
        "planner {}  [{}]",
        lead,
        color::status_label(&view.workflow_status)
    );

    if view.tasks.is_empty() {
        println!("{}  (no delegated tasks yet){}", color::DIM, color::RESET);
    } else {
        for t in &view.tasks {
            println!(
                "|- {}{}#{}  [{}]",
                color::agent(&t.agent_id),
                color::RESET,
                t.task_id,
                color::status_label(&t.status)
            );
            println!("   {}session:{} {}", color::DIM, color::RESET, t.session_id);
        }
    }

    if !view.active_task_ids.is_empty() {
        println!(
            "{}active_task_ids:{} {}",
            color::DIM,
            color::RESET,
            view.active_task_ids.join(", ")
        );
    }
    if !view.recent_events.is_empty() {
        println!();
        println!("{}recent workflow events:{}", color::BOLD, color::RESET);
        for e in &view.recent_events {
            let tid = e
                .task_id
                .as_deref()
                .map(|s| format!(" ({s})"))
                .unwrap_or_default();
            println!(
                "  {}{} {}{} {}",
                color::DIM,
                &e.occurred_at.chars().take(19).collect::<String>(),
                color::RESET,
                e.event_type,
                tid
            );
        }
    }
}

fn print_workflow_graph(view: &WorkflowGraphView, json_output: bool) -> anyhow::Result<()> {
    if json_output {
        println!("{}", serde_json::to_string_pretty(view)?);
    } else {
        print_workflow_graph_text(view);
    }
    Ok(())
}

/// Text tree + recent events from the durable workflow store (`trace graph`).
pub async fn handle_trace_graph(
    config_path: &Path,
    session_or_wf: &str,
    json_output: bool,
    follow: bool,
) -> anyhow::Result<()> {
    use std::io::{stdout, Write};
    use tokio::time::{interval, Duration};

    let config = autonoetic_gateway::config::load_config(config_path)?;
    let workflow_id = resolve_workflow_id_for_graph(&config, session_or_wf)?;

    let run = autonoetic_gateway::scheduler::load_workflow_run(&config, None, &workflow_id)?
        .ok_or_else(|| anyhow::anyhow!("workflow run '{}' disappeared", workflow_id))?;

    if !follow {
        let view = build_workflow_graph_view(&config, &run)?;
        return print_workflow_graph(&view, json_output);
    }

    if !json_output {
        println!(
            "{}Following workflow graph ({}).{} Press Ctrl+C to stop.",
            color::BOLD,
            workflow_id,
            color::RESET
        );
        println!();
    }

    let mut poll_interval = interval(Duration::from_secs(1));
    loop {
        poll_interval.tick().await;
        let run =
            match autonoetic_gateway::scheduler::load_workflow_run(&config, None, &workflow_id)? {
                Some(r) => r,
                None => {
                    tracing::warn!("workflow run removed while following");
                    continue;
                }
            };
        let view = match build_workflow_graph_view(&config, &run) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(error = %e, "rebuild workflow graph view failed");
                continue;
            }
        };
        if json_output {
            println!("{}", serde_json::to_string(&view)?);
        } else {
            print!("\x1b[2J\x1b[H");
            let _ = stdout().flush();
            print_workflow_graph_text(&view);
            println!();
            println!(
                "{}— refreshed — {}Ctrl+C to stop{}",
                color::DIM,
                color::DIM,
                color::RESET
            );
        }
    }
}
