//! Spawn-requested residency and pending-inbound parking.
//!
//! `tests/agent/residency_enablement.rs` pins which *shipped bundles* declare
//! `agent.resident_idle_ttl_secs`. These tests pin the two runtime paths that
//! keep a session addressable when the bundle declares none:
//!
//! - **Spawn flag** (`agent_spawn` arg `resident_idle_ttl_secs`, carried in
//!   spawn metadata): the caller knows it will message the child later, so
//!   the child parks on completion instead of terminating.
//! - **Pending inbound**: a session that closes with undelivered
//!   `agent_message`s queued for it parks briefly, so the wake signal written
//!   at queue time can resume and drain them. Without this a message sent
//!   during the recipient's final turn strands its delivery row forever
//!   (`delivered_at` NULL) on a session that never runs another turn.
//!
//! Both plug into the existing residency machinery — no new lifecycle
//! semantics: a parked session is `Idle`, addressable, and auto-resumed by
//! message wakes.

use std::sync::Arc;

use autonoetic_gateway::execution::GatewayExecutionService;
use autonoetic_gateway::scheduler::gateway_store::{AgentMessageRecord, GatewayStore};
use autonoetic_types::config::GatewayConfig;

use crate::support::{seed_agent_revision, EnvGuard, OpenAiStub, TestWorkspace};

fn install_agent(
    agents_dir: &std::path::Path,
    agent_id: &str,
    residency_line: &str,
) -> anyhow::Result<std::path::PathBuf> {
    let dir = agents_dir.join(agent_id);
    std::fs::create_dir_all(&dir)?;
    std::fs::write(
        dir.join("SKILL.md"),
        format!(
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
  description: "Residency test agent"
{residency_line}
llm_config:
  provider: "openai"
  model: "gpt-4o"
  temperature: 0.0
capabilities:
  - type: "ReadAccess"
    scopes: ["*"]
---
# Instructions
You are a residency test agent. Answer briefly.
"#,
        ),
    )?;
    Ok(dir)
}

fn setup(
    config: &GatewayConfig,
    agents_dir: &std::path::Path,
    agent_id: &str,
    residency_line: &str,
) -> anyhow::Result<Arc<GatewayStore>> {
    let _ = autonoetic_gateway::constitution_digest::initialize_constitution(
        &autonoetic_types::config::GatewayConfig::default(),
    );
    install_agent(agents_dir, agent_id, residency_line)?;
    let gateway_dir = agents_dir.join(".gateway");
    std::fs::create_dir_all(&gateway_dir)?;
    let store = Arc::new(GatewayStore::open(&gateway_dir)?);
    seed_agent_revision(&store, config, agent_id, &agents_dir.join(agent_id))?;
    Ok(store)
}

#[serial_test::serial]
#[test]
fn spawn_resident_flag_parks_completed_session() -> anyhow::Result<()> {
    crate::support::run_with_big_stack(spawn_resident_flag_parks_completed_session_body)
}

async fn spawn_resident_flag_parks_completed_session_body() -> anyhow::Result<()> {
    let workspace = TestWorkspace::new()?;
    let config = workspace.gateway_config();
    let store = setup(&config, &workspace.agents_dir, "resident.flag", "")?;

    let stub = OpenAiStub::spawn(move |_raw, _body| async move {
        serde_json::json!({
            "choices": [{"message": {"content": "done"}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 10, "completion_tokens": 3}
        })
    })
    .await?;
    let _url = EnvGuard::set("AUTONOETIC_LLM_BASE_URL", stub.completion_url());
    let _key = EnvGuard::set("AUTONOETIC_LLM_API_KEY", "test-key");

    let execution = GatewayExecutionService::new(config, Some(store.clone()));
    let metadata = serde_json::json!({
        autonoetic_gateway::execution::SPAWN_RESIDENT_TTL_METADATA_KEY: 900
    });
    let result = execution
        .spawn_agent_once(
            "resident.flag",
            "do something",
            "sess-resident-flag",
            None,
            false,
            None,
            Some(&metadata),
            None,
            None,
            None,
            &[],
        )
        .await?;
    assert!(result.assistant_reply.is_some());

    let residency = store.get_session_residency("sess-resident-flag")?;
    assert!(
        residency.is_some(),
        "spawn flag must park the completed session"
    );
    assert!(store.is_session_addressable("sess-resident-flag")?);
    Ok(())
}

#[serial_test::serial]
#[test]
fn spawn_without_flag_terminates_non_resident_session() -> anyhow::Result<()> {
    crate::support::run_with_big_stack(spawn_without_flag_terminates_non_resident_session_body)
}

async fn spawn_without_flag_terminates_non_resident_session_body() -> anyhow::Result<()> {
    let workspace = TestWorkspace::new()?;
    let config = workspace.gateway_config();
    let store = setup(&config, &workspace.agents_dir, "resident.flag", "")?;

    let stub = OpenAiStub::spawn(move |_raw, _body| async move {
        serde_json::json!({
            "choices": [{"message": {"content": "done"}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 10, "completion_tokens": 3}
        })
    })
    .await?;
    let _url = EnvGuard::set("AUTONOETIC_LLM_BASE_URL", stub.completion_url());
    let _key = EnvGuard::set("AUTONOETIC_LLM_API_KEY", "test-key");

    let execution = GatewayExecutionService::new(config, Some(store.clone()));
    let result = execution
        .spawn_agent_once(
            "resident.flag",
            "do something",
            "sess-no-flag",
            None,
            false,
            None,
            None,
            None,
            None,
            None,
            &[],
        )
        .await?;
    assert!(result.assistant_reply.is_some());

    assert!(store.get_session_residency("sess-no-flag")?.is_none());
    Ok(())
}

#[serial_test::serial]
#[test]
fn pending_inbound_message_parks_non_resident_session_at_close() -> anyhow::Result<()> {
    crate::support::run_with_big_stack(pending_inbound_message_parks_non_resident_session_body)
}

async fn pending_inbound_message_parks_non_resident_session_body() -> anyhow::Result<()> {
    let workspace = TestWorkspace::new()?;
    let config = workspace.gateway_config();
    let store = setup(&config, &workspace.agents_dir, "resident.flag", "")?;

    // Queue the message from inside the stub responder: at that point the
    // session is mid-LLM-call of its final turn — the wake-time drain has
    // already run and no further turn will, which is exactly the race that
    // stranded delivery rows before this fix.
    let store_for_stub = store.clone();
    let stub = OpenAiStub::spawn(move |_raw, _body| {
        let store = store_for_stub.clone();
        async move {
            store
                .save_agent_message(&AgentMessageRecord {
                    message_id: "msg-midturn".to_string(),
                    sender_session_id: "sess-parent".to_string(),
                    sender_agent_id: "planner.default".to_string(),
                    target_pattern: "session:sess-pending-inbound".to_string(),
                    message: "ping".to_string(),
                    created_at: chrono::Utc::now().to_rfc3339(),
                    egress_label: None,
                })
                .unwrap();
            store
                .insert_message_delivery("msg-midturn", "sess-pending-inbound")
                .unwrap();
            serde_json::json!({
                "choices": [{"message": {"content": "done"}, "finish_reason": "stop"}],
                "usage": {"prompt_tokens": 10, "completion_tokens": 3}
            })
        }
    })
    .await?;
    let _url = EnvGuard::set("AUTONOETIC_LLM_BASE_URL", stub.completion_url());
    let _key = EnvGuard::set("AUTONOETIC_LLM_API_KEY", "test-key");

    let execution = GatewayExecutionService::new(config, Some(store.clone()));
    let result = execution
        .spawn_agent_once(
            "resident.flag",
            "do something",
            "sess-pending-inbound",
            None,
            false,
            None,
            None,
            None,
            None,
            None,
            &[],
        )
        .await?;
    assert!(result.assistant_reply.is_some());

    let residency = store.get_session_residency("sess-pending-inbound")?;
    assert!(
        residency.is_some(),
        "a session closing with undelivered inbound messages must park"
    );
    assert!(store.is_session_addressable("sess-pending-inbound")?);
    Ok(())
}

#[serial_test::serial]
#[test]
fn manifest_residency_wins_over_spawn_flag() -> anyhow::Result<()> {
    crate::support::run_with_big_stack(manifest_residency_wins_over_spawn_flag_body)
}

async fn manifest_residency_wins_over_spawn_flag_body() -> anyhow::Result<()> {
    let workspace = TestWorkspace::new()?;
    let config = workspace.gateway_config();
    let store = setup(
        &config,
        &workspace.agents_dir,
        "resident.flag",
        "  resident_idle_ttl_secs: 1200\n",
    )?;

    let stub = OpenAiStub::spawn(move |_raw, _body| async move {
        serde_json::json!({
            "choices": [{"message": {"content": "done"}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 10, "completion_tokens": 3}
        })
    })
    .await?;
    let _url = EnvGuard::set("AUTONOETIC_LLM_BASE_URL", stub.completion_url());
    let _key = EnvGuard::set("AUTONOETIC_LLM_API_KEY", "test-key");

    let execution = GatewayExecutionService::new(config, Some(store.clone()));
    let metadata = serde_json::json!({
        autonoetic_gateway::execution::SPAWN_RESIDENT_TTL_METADATA_KEY: 60
    });
    let result = execution
        .spawn_agent_once(
            "resident.flag",
            "do something",
            "sess-manifest-wins",
            None,
            false,
            None,
            Some(&metadata),
            None,
            None,
            None,
            &[],
        )
        .await?;
    assert!(result.assistant_reply.is_some());

    let residency = store
        .get_session_residency("sess-manifest-wins")?
        .expect("manifest-declared residency must park the session");
    let expires = chrono::DateTime::parse_from_rfc3339(&residency.expires_at)?;
    let remaining = expires.signed_duration_since(chrono::Utc::now());
    assert!(
        remaining.num_seconds() > 600,
        "manifest TTL (1200s) must win over the spawn flag (60s); remaining: {remaining}"
    );
    Ok(())
}
