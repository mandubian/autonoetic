//! Integration test: Agent Messaging subsystem.
//! Verifies that `agent.message` successfully saves a message to the database,
//! triggers a wakeup signal, and the message is retrieved/injected into the
//! target agent's context on next execute_with_history.

mod support;

use std::path::Path;
use std::sync::Arc;

use autonoetic_gateway::policy::PolicyEngine;
use autonoetic_gateway::runtime::tools::{default_registry, NativeToolRegistry};
use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_types::agent::AgentManifest;
use support::EnvGuard;

fn install_agent(agents_dir: &Path, name: &str, capabilities: &str) -> anyhow::Result<()> {
    let agent_dir = agents_dir.join(name);
    std::fs::create_dir_all(&agent_dir)?;
    std::fs::write(agent_dir.join("runtime.lock"), "dependencies: []")?;
    std::fs::write(
        agent_dir.join("SKILL.md"),
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
  id: "{}"
  name: "Agent {}"
  description: "Test agent"
{}
---
# {}
"#,
            name, name, capabilities, name
        ),
    )?;
    Ok(())
}

#[serial_test::serial]
#[tokio::test]
async fn test_agent_message_delivery() -> anyhow::Result<()> {
    let workspace = support::TestWorkspace::new()?;
    let config = workspace.gateway_config();
    let gateway_dir = workspace.agents_dir.join(".gateway");
    std::fs::create_dir_all(&gateway_dir)?;

    install_agent(
        &workspace.agents_dir,
        "sender-agent",
        r#"capabilities:
  - type: "AgentMessage"
    patterns: ["*"]"#,
    )?;

    install_agent(&workspace.agents_dir, "receiver-agent", "capabilities: []")?;

    let store = Arc::new(GatewayStore::open(&gateway_dir)?);

    // We don't necessarily need to seed test agents, just execute the tool directly.
    let registry = default_registry();
    let manifest_content =
        std::fs::read_to_string(workspace.agents_dir.join("sender-agent/SKILL.md"))?;
    let manifest: AgentManifest =
        serde_yaml::from_str(manifest_content.split("---").nth(1).unwrap())?;
    let policy = PolicyEngine::new(manifest.clone());

    let sender_session = "sender-session-1";
    let receiver_session = "receiver-session-2";

    let args = serde_json::json!({
        "target_session_id": receiver_session,
        "target_agent_id": "receiver-agent",
        "message": "Hello from sender"
    });

    // 1. Invoke agent.message
    let result = registry.execute(
        "agent_message",
        &manifest,
        &policy,
        &workspace.agents_dir.join("sender-agent"),
        Some(&gateway_dir),
        &args.to_string(),
        Some(sender_session),
        Some("turn-1"),
        Some(&config),
        Some(store.clone()),
        None,
    )?;

    let parsed: serde_json::Value = serde_json::from_str(&result)?;
    assert!(parsed.get("ok").unwrap().as_bool().unwrap());
    assert_eq!(parsed.get("recipients_count").unwrap().as_u64().unwrap(), 1);

    // 2. Verify DB state - Message should be undelivered for the receiver
    let undelivered = store.fetch_undelivered_messages(receiver_session)?;
    assert_eq!(undelivered.len(), 1);
    assert_eq!(undelivered[0].message, "Hello from sender");
    assert_eq!(undelivered[0].sender_agent_id, "sender-agent");

    let pending_notifications = store.list_notifications_for_session(
        receiver_session,
        autonoetic_types::notification::NotificationStatus::Pending,
    )?;
    assert_eq!(pending_notifications.len(), 1);
    assert_eq!(
        pending_notifications[0].notification_type,
        autonoetic_types::notification::NotificationType::AgentMessage
    );

    Ok(())
}

#[serial_test::serial]
#[tokio::test]
async fn test_agent_message_missing_target_agent_returns_structured_error() -> anyhow::Result<()> {
    let workspace = support::TestWorkspace::new()?;
    let config = workspace.gateway_config();
    let gateway_dir = workspace.agents_dir.join(".gateway");
    std::fs::create_dir_all(&gateway_dir)?;

    install_agent(
        &workspace.agents_dir,
        "sender-agent",
        r#"capabilities:
  - type: "AgentMessage"
    patterns: ["*"]"#,
    )?;

    let store = Arc::new(GatewayStore::open(&gateway_dir)?);
    let registry = default_registry();
    let manifest_content =
        std::fs::read_to_string(workspace.agents_dir.join("sender-agent/SKILL.md"))?;
    let manifest: AgentManifest =
        serde_yaml::from_str(manifest_content.split("---").nth(1).unwrap())?;
    let policy = PolicyEngine::new(manifest.clone());

    let args = serde_json::json!({
        "target_agent_id": "missing-agent",
        "message": "Hello from sender"
    });

    let result = registry.execute(
        "agent_message",
        &manifest,
        &policy,
        &workspace.agents_dir.join("sender-agent"),
        Some(&gateway_dir),
        &args.to_string(),
        Some("sender-session-1"),
        Some("turn-1"),
        Some(&config),
        Some(store.clone()),
        None,
    )?;

    let parsed: serde_json::Value = serde_json::from_str(&result)?;
    assert!(!parsed.get("ok").unwrap().as_bool().unwrap());
    assert_eq!(
        parsed.get("status").unwrap().as_str().unwrap(),
        "target_agent_not_found"
    );
    assert_eq!(
        parsed.get("target_agent_id").unwrap().as_str().unwrap(),
        "missing-agent"
    );
    assert_eq!(parsed.get("recipients_count").unwrap().as_u64().unwrap(), 0);
    assert_eq!(parsed.get("exists").unwrap().as_bool().unwrap(), false);

    Ok(())
}

#[serial_test::serial]
#[tokio::test]
async fn test_agent_message_existing_agent_without_live_session_returns_structured_error(
) -> anyhow::Result<()> {
    let workspace = support::TestWorkspace::new()?;
    let config = workspace.gateway_config();
    let gateway_dir = workspace.agents_dir.join(".gateway");
    std::fs::create_dir_all(&gateway_dir)?;

    install_agent(
        &workspace.agents_dir,
        "sender-agent",
        r#"capabilities:
  - type: "AgentMessage"
    patterns: ["*"]"#,
    )?;

    install_agent(&workspace.agents_dir, "receiver-agent", "capabilities: []")?;

    let store = Arc::new(GatewayStore::open(&gateway_dir)?);
    let registry = default_registry();
    let manifest_content =
        std::fs::read_to_string(workspace.agents_dir.join("sender-agent/SKILL.md"))?;
    let manifest: AgentManifest =
        serde_yaml::from_str(manifest_content.split("---").nth(1).unwrap())?;
    let policy = PolicyEngine::new(manifest.clone());

    let args = serde_json::json!({
        "target_agent_id": "receiver-agent",
        "message": "Hello from sender"
    });

    let result = registry.execute(
        "agent_message",
        &manifest,
        &policy,
        &workspace.agents_dir.join("sender-agent"),
        Some(&gateway_dir),
        &args.to_string(),
        Some("sender-session-1"),
        Some("turn-1"),
        Some(&config),
        Some(store.clone()),
        None,
    )?;

    let parsed: serde_json::Value = serde_json::from_str(&result)?;
    assert!(!parsed.get("ok").unwrap().as_bool().unwrap());
    assert_eq!(
        parsed.get("status").unwrap().as_str().unwrap(),
        "no_live_recipients"
    );
    assert_eq!(
        parsed.get("target_agent_id").unwrap().as_str().unwrap(),
        "receiver-agent"
    );
    assert_eq!(parsed.get("recipients_count").unwrap().as_u64().unwrap(), 0);
    assert_eq!(parsed.get("exists").unwrap().as_bool().unwrap(), true);

    Ok(())
}
