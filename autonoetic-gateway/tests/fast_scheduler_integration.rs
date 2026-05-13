//! Integration tests for the fast scheduler sidecar (issue #8).
//!
//! Covers:
//! - eligibility filter (interval-only jobs go through; cron-style jobs skipped)
//! - successful dispatch of a script-mode interval job via the fast path
//! - dispatch-boundary guardrail: sub-10s interval with non-script target is
//!   cancelled defensively
//! - race-safe dedup: concurrent fast-tick invocations enqueue at most once
//! - paused jobs are not dispatched

mod support;

use std::sync::atomic::Ordering;
use std::sync::Arc;

use autonoetic_gateway::scheduler::fast_scheduler::{
    run_fast_scheduler_tick_at, FastSchedulerStats,
};
use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_gateway::scheduler::workflow_store;
use autonoetic_gateway::GatewayExecutionService;
use autonoetic_types::config::{FastSchedulerConfig, GatewayConfig};
use autonoetic_types::scheduled_job::{ScheduledJob, ScheduledJobStatus};
use chrono::{Duration, Utc};

use support::{seed_agent_revision, TestWorkspace};

fn install_script_agent(agent_dir: &std::path::Path, agent_id: &str) -> anyhow::Result<()> {
    std::fs::create_dir_all(agent_dir.join("scripts"))?;
    std::fs::write(
        agent_dir.join("scripts/noop.py"),
        "#!/usr/bin/env python3\nprint('{}')\n",
    )?;
    let skill_md = format!(
        r#"---
version: "1.0"
runtime:
  engine: "autonoetic"
  gateway_version: "0.1.0"
  sdk_version: "0.1.0"
  type: "stateful"
  sandbox: "bubblewrap"
  runtime_lock: "runtime.lock"
agent:
  id: "{agent_id}"
  name: "{agent_id}"
  description: "Script agent for fast scheduler test"
execution_mode: script
script_entry: scripts/noop.py
capabilities: []
---
# Script Agent
"#,
    );
    std::fs::write(agent_dir.join("SKILL.md"), skill_md)?;
    std::fs::write(agent_dir.join("runtime.lock"), "dependencies: []")?;
    Ok(())
}

fn install_reasoning_agent(agent_dir: &std::path::Path, agent_id: &str) -> anyhow::Result<()> {
    let skill_md = format!(
        r#"---
version: "1.0"
runtime:
  engine: "autonoetic"
  gateway_version: "0.1.0"
  sdk_version: "0.1.0"
  type: "stateful"
  sandbox: "bubblewrap"
  runtime_lock: "runtime.lock"
agent:
  id: "{agent_id}"
  name: "{agent_id}"
  description: "Reasoning agent for fast scheduler guardrail test"
capabilities: []
---
# Reasoning Agent
"#,
    );
    std::fs::create_dir_all(agent_dir)?;
    std::fs::write(agent_dir.join("SKILL.md"), skill_md)?;
    std::fs::write(agent_dir.join("runtime.lock"), "dependencies: []")?;
    Ok(())
}

fn fast_config(workspace: &TestWorkspace) -> GatewayConfig {
    let mut cfg = workspace.gateway_config();
    cfg.fast_scheduler = FastSchedulerConfig {
        enabled: true,
        tick_millis: 100,
        window_secs: 2,
        max_due_per_tick: 64,
    };
    cfg
}

fn make_job(
    job_id: &str,
    owner: &str,
    target: &str,
    target_revision_id: &str,
    cron_expr: &str,
    next_run_at_offset_secs: i64,
) -> ScheduledJob {
    let now = Utc::now();
    let next_run = now + Duration::seconds(next_run_at_offset_secs);
    ScheduledJob {
        job_id: job_id.to_string(),
        owner_agent_id: owner.to_string(),
        root_session_id: format!("root-{job_id}"),
        target_agent_id: target.to_string(),
        target_revision_id: target_revision_id.to_string(),
        message: "tick".to_string(),
        metadata_json: None,
        cron_expr: cron_expr.to_string(),
        timezone: "UTC".to_string(),
        next_run_at: next_run.to_rfc3339(),
        last_run_at: None,
        status: ScheduledJobStatus::Active,
        created_at: now.to_rfc3339(),
        updated_at: now.to_rfc3339(),
        last_error: None,
        generation: 0,
    }
}

#[tokio::test]
async fn fast_path_dispatches_due_interval_job_for_script_target() -> anyhow::Result<()> {
    let workspace = TestWorkspace::new()?;
    let config = fast_config(&workspace);

    let gateway_dir = config.agents_dir.join(".gateway");
    std::fs::create_dir_all(&gateway_dir)?;
    let store = Arc::new(GatewayStore::open(&gateway_dir)?);

    let agent_id = "fast-script-agent";
    install_script_agent(&config.agents_dir.join(agent_id), agent_id)?;
    let revision_id = seed_agent_revision(
        store.as_ref(),
        &config,
        agent_id,
        &config.agents_dir.join(agent_id),
    )?;

    let job = make_job(
        "fast-job-1",
        "planner.default",
        agent_id,
        &revision_id,
        "every 2 seconds",
        -1,
    );
    store.create_scheduled_job(&job)?;

    let execution = Arc::new(GatewayExecutionService::new(
        config.clone(),
        Some(store.clone()),
    ));
    let stats = Arc::new(FastSchedulerStats::default());

    run_fast_scheduler_tick_at(execution, Utc::now(), stats.clone()).await?;

    let snap = stats.snapshot();
    assert_eq!(snap.fast_due_loaded, 1, "one candidate loaded");
    assert_eq!(snap.fast_claimed, 1, "one job claimed");
    assert_eq!(snap.fast_enqueued, 1, "one workflow task enqueued");
    assert_eq!(snap.fast_claim_miss, 0);
    assert_eq!(snap.fast_enqueue_failed, 0);

    let queued = workflow_store::load_queued_tasks(&config, Some(store.as_ref()), "sched-fast-job-1")?;
    assert_eq!(queued.len(), 1, "exactly one queued task");
    let task = &queued[0];
    assert_eq!(task.agent_id, format!("{agent_id}@{revision_id}"));
    assert_eq!(
        task.metadata.as_ref().and_then(|m| m.get("fast_path")),
        Some(&serde_json::Value::Bool(true))
    );

    let advanced = store.get_scheduled_job("fast-job-1")?.unwrap();
    assert!(
        advanced.next_run_at > job.next_run_at,
        "next_run_at must advance"
    );

    Ok(())
}

#[tokio::test]
async fn fast_path_skips_cron_style_schedules() -> anyhow::Result<()> {
    let workspace = TestWorkspace::new()?;
    let config = fast_config(&workspace);

    let gateway_dir = config.agents_dir.join(".gateway");
    std::fs::create_dir_all(&gateway_dir)?;
    let store = Arc::new(GatewayStore::open(&gateway_dir)?);

    let agent_id = "fast-script-agent";
    install_script_agent(&config.agents_dir.join(agent_id), agent_id)?;
    let revision_id = seed_agent_revision(
        store.as_ref(),
        &config,
        agent_id,
        &config.agents_dir.join(agent_id),
    )?;

    let job = make_job(
        "cron-job-1",
        "planner.default",
        agent_id,
        &revision_id,
        "*/5 * * * *",
        -1,
    );
    store.create_scheduled_job(&job)?;

    let execution = Arc::new(GatewayExecutionService::new(
        config.clone(),
        Some(store.clone()),
    ));
    let stats = Arc::new(FastSchedulerStats::default());

    run_fast_scheduler_tick_at(execution, Utc::now(), stats.clone()).await?;

    let snap = stats.snapshot();
    assert_eq!(snap.fast_due_loaded, 1, "candidate loaded");
    assert_eq!(snap.fast_claimed, 0, "cron-style schedule must not be claimed");
    assert_eq!(snap.fast_enqueued, 0);

    let unchanged = store.get_scheduled_job("cron-job-1")?.unwrap();
    assert_eq!(unchanged.generation, 0, "generation untouched");
    assert_eq!(unchanged.next_run_at, job.next_run_at);

    Ok(())
}

#[tokio::test]
async fn fast_path_cancels_sub10s_job_when_target_not_script_mode() -> anyhow::Result<()> {
    let workspace = TestWorkspace::new()?;
    let config = fast_config(&workspace);

    let gateway_dir = config.agents_dir.join(".gateway");
    std::fs::create_dir_all(&gateway_dir)?;
    let store = Arc::new(GatewayStore::open(&gateway_dir)?);

    let agent_id = "fast-reasoning-agent";
    install_reasoning_agent(&config.agents_dir.join(agent_id), agent_id)?;
    let revision_id = seed_agent_revision(
        store.as_ref(),
        &config,
        agent_id,
        &config.agents_dir.join(agent_id),
    )?;

    // Direct-write the job to simulate either a malicious DB write or a
    // post-creation manifest mutation — the creation-time guardrail in
    // `runtime/tools/scheduler.rs` would normally block this at the tool
    // surface.
    let job = make_job(
        "reasoning-3s",
        "planner.default",
        agent_id,
        &revision_id,
        "every 3 seconds",
        -1,
    );
    store.create_scheduled_job(&job)?;

    let execution = Arc::new(GatewayExecutionService::new(
        config.clone(),
        Some(store.clone()),
    ));
    let stats = Arc::new(FastSchedulerStats::default());

    run_fast_scheduler_tick_at(execution, Utc::now(), stats.clone()).await?;

    let snap = stats.snapshot();
    assert_eq!(snap.fast_due_loaded, 1);
    assert_eq!(
        snap.fast_claimed, 0,
        "sub-10s reasoning job must not be claimed"
    );
    assert_eq!(snap.fast_enqueued, 0);

    let after = store.get_scheduled_job("reasoning-3s")?.unwrap();
    assert_eq!(
        after.status,
        ScheduledJobStatus::Cancelled,
        "guardrail cancels offending job"
    );

    Ok(())
}

#[tokio::test]
async fn fast_path_concurrent_ticks_do_not_double_enqueue() -> anyhow::Result<()> {
    let workspace = TestWorkspace::new()?;
    let config = fast_config(&workspace);

    let gateway_dir = config.agents_dir.join(".gateway");
    std::fs::create_dir_all(&gateway_dir)?;
    let store = Arc::new(GatewayStore::open(&gateway_dir)?);

    let agent_id = "fast-script-agent";
    install_script_agent(&config.agents_dir.join(agent_id), agent_id)?;
    let revision_id = seed_agent_revision(
        store.as_ref(),
        &config,
        agent_id,
        &config.agents_dir.join(agent_id),
    )?;

    let job = make_job(
        "race-job-1",
        "planner.default",
        agent_id,
        &revision_id,
        "every 2 seconds",
        -1,
    );
    store.create_scheduled_job(&job)?;

    let execution = Arc::new(GatewayExecutionService::new(
        config.clone(),
        Some(store.clone()),
    ));
    let stats_a = Arc::new(FastSchedulerStats::default());
    let stats_b = Arc::new(FastSchedulerStats::default());

    let now = Utc::now();
    let exec_a = execution.clone();
    let exec_b = execution.clone();
    let stats_a_c = stats_a.clone();
    let stats_b_c = stats_b.clone();
    let (a, b) = tokio::join!(
        async move { run_fast_scheduler_tick_at(exec_a, now, stats_a_c).await },
        async move { run_fast_scheduler_tick_at(exec_b, now, stats_b_c).await },
    );
    a?;
    b?;

    let total_enqueued =
        stats_a.fast_enqueued.load(Ordering::Relaxed) + stats_b.fast_enqueued.load(Ordering::Relaxed);
    let total_claimed =
        stats_a.fast_claimed.load(Ordering::Relaxed) + stats_b.fast_claimed.load(Ordering::Relaxed);
    let total_miss =
        stats_a.fast_claim_miss.load(Ordering::Relaxed) + stats_b.fast_claim_miss.load(Ordering::Relaxed);

    assert_eq!(total_claimed, 1, "exactly one claim across both ticks");
    assert_eq!(total_enqueued, 1, "exactly one enqueue across both ticks");
    assert!(total_miss >= 1, "the loser sees a claim_miss");

    let queued =
        workflow_store::load_queued_tasks(&config, Some(store.as_ref()), "sched-race-job-1")?;
    assert_eq!(queued.len(), 1, "no double-dispatch in queue");

    Ok(())
}

#[tokio::test]
async fn fast_path_skips_paused_jobs() -> anyhow::Result<()> {
    let workspace = TestWorkspace::new()?;
    let config = fast_config(&workspace);

    let gateway_dir = config.agents_dir.join(".gateway");
    std::fs::create_dir_all(&gateway_dir)?;
    let store = Arc::new(GatewayStore::open(&gateway_dir)?);

    let agent_id = "fast-script-agent";
    install_script_agent(&config.agents_dir.join(agent_id), agent_id)?;
    let revision_id = seed_agent_revision(
        store.as_ref(),
        &config,
        agent_id,
        &config.agents_dir.join(agent_id),
    )?;

    let job = make_job(
        "paused-job-1",
        "planner.default",
        agent_id,
        &revision_id,
        "every 2 seconds",
        -1,
    );
    store.create_scheduled_job(&job)?;
    store.pause_scheduled_job("paused-job-1")?;

    let execution = Arc::new(GatewayExecutionService::new(
        config.clone(),
        Some(store.clone()),
    ));
    let stats = Arc::new(FastSchedulerStats::default());

    run_fast_scheduler_tick_at(execution, Utc::now(), stats.clone()).await?;

    let snap = stats.snapshot();
    assert_eq!(
        snap.fast_due_loaded, 0,
        "paused jobs are not eligible (status='active' filter)"
    );
    assert_eq!(snap.fast_enqueued, 0);

    Ok(())
}
