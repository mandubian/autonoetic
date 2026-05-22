use std::path::Path;
use std::sync::Arc;

use anyhow::Context;
use tracing::info;

use autonoetic_gateway::llm::{build_driver, Message};
use autonoetic_gateway::runtime::lifecycle::{AgentExecutor, TurnOutcome};
use autonoetic_gateway::runtime::tools::default_registry;
use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_gateway::AgentRepository;

/// Structured result from a single watchdog invocation. Consumed by the
/// `sentinel-experiment` harness; the user-facing CLI just prints `reply`.
pub struct WatchdogRun {
    pub reply: Option<String>,
    /// Human-readable description of how the run terminated. `Completed`
    /// for normal termination, otherwise an `[interrupted]` / `[suspended]`
    /// tag with detail. Used by the harness to classify abnormal runs.
    pub outcome_tag: String,
}

/// Programmatic entry point. Builds an isolated watchdog executor against
/// the given session_id, runs one execute_with_history pass, and returns
/// the captured reply.
///
/// The CLI wrapper [`handle_watchdog`] forwards to this and prints the
/// reply. The experiment harness uses this directly so it can capture
/// the reply and classify it.
pub async fn run_watchdog(
    config_path: &Path,
    session_id: &str,
) -> anyhow::Result<WatchdogRun> {
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
        .context("Watchdog agent 'watchdog.default' not found — run with a config that points to the agents directory containing agents/specialists/watchdog.default/")?;
    let manifest = loaded.manifest;
    let instructions = loaded.instructions;
    let agent_dir = loaded.dir;

    let llm_config = manifest
        .llm_config
        .clone()
        .context("watchdog.default is missing llm_config in SKILL.md")?;
    let driver = build_driver(llm_config, reqwest::Client::new())?;

    // Surface the latest Layer 1 trajectory health snapshot to the watchdog
    // so it does not re-derive what the deterministic monitor already computed.
    // We look back for the most recent `divergence.*` causal event on the
    // target session and include its level + signal evidence in the kickoff
    // message. Falls back gracefully when no events are present.
    let trajectory_snapshot = build_trajectory_snapshot_summary(&store, session_id);

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
             \n\
             {}\n\
             \n\
             Use digest_query and execution_search to gather additional evidence \
             if the snapshot above is missing or stale, then produce a judgment. \
             If you find critical divergence, escalate via session_escalate with \
             high urgency.",
            session_id, trajectory_snapshot,
        ));

    let mut history = vec![
        Message::system(runtime.instructions.clone()),
        Message::user(runtime.initial_user_message.clone()),
    ];

    let result = match runtime.execute_with_history(&mut history).await {
        Ok(TurnOutcome::Completed(reply)) => WatchdogRun {
            reply,
            outcome_tag: "completed".to_string(),
        },
        Ok(TurnOutcome::Suspended { approval_request_id, .. }) => WatchdogRun {
            reply: None,
            outcome_tag: format!("suspended_for_approval:{}", approval_request_id),
        },
        Ok(TurnOutcome::SuspendedUserInput { interaction_id }) => WatchdogRun {
            reply: None,
            outcome_tag: format!("waiting_for_input:{}", interaction_id),
        },
        Ok(other) => WatchdogRun {
            reply: None,
            outcome_tag: format!("interrupted:{:?}", other),
        },
        Err(e) => WatchdogRun {
            reply: None,
            outcome_tag: format!("error:{}", e),
        },
    };

    Ok(result)
}

/// Run the divergence watchdog against a target session and print the
/// reply. Thin wrapper over [`run_watchdog`].
pub async fn handle_watchdog(config_path: &Path, session_id: &str) -> anyhow::Result<()> {
    let run = run_watchdog(config_path, session_id).await?;
    match run.reply {
        Some(reply) => println!("{}", reply),
        None => println!("[Watchdog produced no reply — outcome: {}]", run.outcome_tag),
    }
    Ok(())
}

/// Build a short, human-readable trajectory snapshot for the kickoff
/// message. Pulls the most recent `divergence.*` causal event from the
/// store and renders its level + signal evidence. Returns a fallback
/// string when no events are found so the watchdog still has a stable
/// kickoff structure.
fn build_trajectory_snapshot_summary(
    store: &Arc<GatewayStore>,
    target_session_id: &str,
) -> String {
    match store.search_causal_events(Some(target_session_id), None, 200) {
        Ok(events) => {
            let latest = events
                .iter()
                .find(|e| e.category == "divergence");
            match latest {
                Some(event) => {
                    let level = event
                        .payload
                        .as_ref()
                        .and_then(|p| serde_json::from_str::<serde_json::Value>(p).ok())
                        .and_then(|v| v.get("level").and_then(|l| l.as_str()).map(|s| s.to_string()))
                        .unwrap_or_else(|| event.action.clone());
                    let evidence: Vec<String> = event
                        .payload
                        .as_ref()
                        .and_then(|p| serde_json::from_str::<serde_json::Value>(p).ok())
                        .and_then(|v| {
                            v.get("signals")
                                .and_then(|s| s.as_array())
                                .map(|arr| {
                                    arr.iter()
                                        .filter_map(|sig| {
                                            let kind = sig.get("kind")?.as_str()?;
                                            let severity = sig.get("severity")?.as_str()?;
                                            let ev = sig
                                                .get("evidence")
                                                .and_then(|e| e.as_str())
                                                .unwrap_or("");
                                            Some(format!("  - {} ({}): {}", kind, severity, ev))
                                        })
                                        .collect::<Vec<_>>()
                                })
                        })
                        .unwrap_or_default();
                    if evidence.is_empty() {
                        format!(
                            "Layer 1 snapshot (most recent divergence event at {}):\n  level = {}",
                            event.timestamp, level
                        )
                    } else {
                        format!(
                            "Layer 1 snapshot (most recent divergence event at {}):\n  level = {}\n  signals:\n{}",
                            event.timestamp,
                            level,
                            evidence.join("\n")
                        )
                    }
                }
                None => {
                    "Layer 1 snapshot: no divergence.* events recorded for this session yet. \
                     The deterministic monitor either has not flagged anything or has not run; \
                     gather evidence from scratch."
                        .to_string()
                }
            }
        }
        Err(e) => format!(
            "Layer 1 snapshot: unavailable (causal-event query failed: {}). \
             Proceed by gathering evidence from scratch.",
            e
        ),
    }
}
