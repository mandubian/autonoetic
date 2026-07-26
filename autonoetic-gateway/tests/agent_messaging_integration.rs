//! Integration test: Agent Messaging subsystem (`agent_message`).
//!
//! Covers the two addressing modes and their failure statuses:
//!
//! - `target_session_id` — one session, ACL-checked against that session's
//!   bound agent so a narrow `AgentMessage` grant cannot be widened by naming a
//!   session id directly.
//! - `target_agent_id` — broadcast to the role's *unfinished* sessions only,
//!   never looping back to the sender.
//!
//! Delivery itself is a queued `agent_message_deliveries` row plus a pending
//! notification; the lifecycle drains that queue at the receiver's next wake.

mod support;

use std::path::Path;
use std::sync::Arc;

use autonoetic_gateway::policy::PolicyEngine;
use autonoetic_gateway::runtime::tools::{default_registry, NativeToolRegistry};
use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_types::agent::AgentManifest;
use autonoetic_types::agent_revision::SessionAgentBinding;

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
  id: "{name}"
  name: "Agent {name}"
  description: "Test agent"
{capabilities}
---
# {name}
"#
        ),
    )?;
    Ok(())
}

/// An `AgentMessage` capability scoped to `patterns`.
fn messaging_caps(patterns: &str) -> String {
    format!("capabilities:\n  - type: \"AgentMessage\"\n    patterns: [{patterns}]")
}

fn load_manifest(agents_dir: &Path, name: &str) -> anyhow::Result<AgentManifest> {
    let raw = std::fs::read_to_string(agents_dir.join(name).join("SKILL.md"))?;
    Ok(serde_yaml::from_str(
        raw.split("---").nth(1).expect("frontmatter"),
    )?)
}

/// Bind `session_id` to `agent_id`. Any real session has such a row; the
/// messaging tool uses it both to resolve the ACL target and to enumerate a
/// role's sessions.
fn bind_session(store: &GatewayStore, session_id: &str, agent_id: &str) -> anyhow::Result<()> {
    store.upsert_session_agent_binding(&SessionAgentBinding {
        session_id: session_id.to_string(),
        root_session_id: session_id.to_string(),
        alias_id: None,
        agent_id: agent_id.to_string(),
        revision_id: "rev-1".to_string(),
        runtime_lock_hash: "hash".to_string(),
        home_node_id: "node".to_string(),
        created_at: "2026-01-01T00:00:00Z".to_string(),
        requested_target: agent_id.to_string(),
        constitution_version: None,
        constitution_digest: None,
    })?;
    Ok(())
}

/// Mark a bound session terminal the way a real close does: an unconditional
/// `session_outcomes` row (see `session_outcome_writer`).
fn finish_session(store: &GatewayStore, session_id: &str, agent_id: &str) -> anyhow::Result<()> {
    store.upsert_session_outcome_metrics(session_id, session_id, agent_id, None, 3, 100, 0.01, 1.0)?;
    Ok(())
}

struct Harness {
    _workspace: support::TestWorkspace,
    config: autonoetic_types::config::GatewayConfig,
    agents_dir: std::path::PathBuf,
    gateway_dir: std::path::PathBuf,
    store: Arc<GatewayStore>,
    registry: NativeToolRegistry,
}

impl Harness {
    /// Installs `sender-agent` (with `sender_patterns`) and `receiver-agent`.
    fn new(sender_patterns: &str) -> anyhow::Result<Self> {
        let workspace = support::TestWorkspace::new()?;
        let config = workspace.gateway_config();
        let agents_dir = workspace.agents_dir.clone();
        let gateway_dir = agents_dir.join(".gateway");
        std::fs::create_dir_all(&gateway_dir)?;

        install_agent(&agents_dir, "sender-agent", &messaging_caps(sender_patterns))?;
        install_agent(&agents_dir, "receiver-agent", "capabilities: []")?;

        let store = Arc::new(GatewayStore::open(&gateway_dir)?);
        Ok(Self {
            _workspace: workspace,
            config,
            agents_dir,
            gateway_dir,
            store,
            registry: default_registry(),
        })
    }

    fn send_as(
        &self,
        sender_agent: &str,
        sender_session: &str,
        args: serde_json::Value,
    ) -> anyhow::Result<String> {
        let manifest = load_manifest(&self.agents_dir, sender_agent)?;
        let policy = PolicyEngine::new(manifest.clone());
        self.registry.execute(
            "agent_message",
            &manifest,
            &policy,
            &self.agents_dir.join(sender_agent),
            Some(&self.gateway_dir),
            &args.to_string(),
            Some(sender_session),
            Some("turn-1"),
            Some(&self.config),
            Some(self.store.clone()),
            None,
        )
    }

    fn send(&self, args: serde_json::Value) -> anyhow::Result<serde_json::Value> {
        let raw = self.send_as("sender-agent", "sender-session-1", args)?;
        Ok(serde_json::from_str(&raw)?)
    }

    fn queued_for(&self, session_id: &str) -> anyhow::Result<usize> {
        Ok(self.store.fetch_undelivered_messages(session_id)?.len())
    }
}

#[serial_test::serial]
#[tokio::test]
async fn test_agent_message_delivery() -> anyhow::Result<()> {
    let h = Harness::new("\"*\"")?;
    let receiver_session = "receiver-session-2";
    bind_session(&h.store, receiver_session, "receiver-agent")?;

    let parsed = h.send(serde_json::json!({
        "target_session_id": receiver_session,
        "message": "Hello from sender"
    }))?;

    assert!(parsed["ok"].as_bool().unwrap());
    assert_eq!(parsed["status"].as_str().unwrap(), "delivered");
    assert_eq!(parsed["recipients_count"].as_u64().unwrap(), 1);

    let undelivered = h.store.fetch_undelivered_messages(receiver_session)?;
    assert_eq!(undelivered.len(), 1);
    assert_eq!(undelivered[0].message, "Hello from sender");
    assert_eq!(undelivered[0].sender_agent_id, "sender-agent");

    let pending = h.store.list_notifications_for_session(
        receiver_session,
        autonoetic_types::notification::NotificationStatus::Pending,
    )?;
    assert_eq!(pending.len(), 1);
    assert_eq!(
        pending[0].notification_type,
        autonoetic_types::notification::NotificationType::AgentMessage
    );

    Ok(())
}

/// A broadcast must reach only sessions that can still consume a delivery.
///
/// `session_agent_bindings` is append-only, so enumerating it unfiltered
/// reported every session the role had *ever* run as a live recipient —
/// `recipients_count` was a count of history, and the pump would try to wake
/// each dead session.
#[serial_test::serial]
#[tokio::test]
async fn broadcast_reaches_only_unfinished_sessions() -> anyhow::Result<()> {
    let h = Harness::new("\"*\"")?;

    for sid in ["old-session-1", "old-session-2"] {
        bind_session(&h.store, sid, "receiver-agent")?;
        finish_session(&h.store, sid, "receiver-agent")?;
    }
    bind_session(&h.store, "live-session", "receiver-agent")?;

    let parsed = h.send(serde_json::json!({
        "target_agent_id": "receiver-agent",
        "message": "only the live one should get this"
    }))?;

    assert!(parsed["ok"].as_bool().unwrap());
    assert_eq!(
        parsed["recipients_count"].as_u64().unwrap(),
        1,
        "finished sessions must not be counted as recipients: {parsed}"
    );
    assert_eq!(h.queued_for("live-session")?, 1);
    assert_eq!(h.queued_for("old-session-1")?, 0);
    assert_eq!(h.queued_for("old-session-2")?, 0);

    Ok(())
}

/// Broadcasting to your own role must not deliver to yourself — a self-message
/// would be injected into the very turn that produced it.
#[serial_test::serial]
#[tokio::test]
async fn broadcast_excludes_the_senders_own_session() -> anyhow::Result<()> {
    let h = Harness::new("\"*\"")?;
    bind_session(&h.store, "sender-session-1", "sender-agent")?;
    bind_session(&h.store, "peer-session-2", "sender-agent")?;

    let parsed = h.send(serde_json::json!({
        "target_agent_id": "sender-agent",
        "message": "broadcast to my own role"
    }))?;

    assert_eq!(parsed["recipients_count"].as_u64().unwrap(), 1);
    assert_eq!(h.queued_for("peer-session-2")?, 1);
    assert_eq!(
        h.queued_for("sender-session-1")?,
        0,
        "sender must not receive its own broadcast: {parsed}"
    );

    Ok(())
}

/// Sole live session being the sender's own is the same as having none.
#[serial_test::serial]
#[tokio::test]
async fn broadcast_to_own_role_with_no_peer_reports_no_live_recipients() -> anyhow::Result<()> {
    let h = Harness::new("\"*\"")?;
    bind_session(&h.store, "sender-session-1", "sender-agent")?;

    let parsed = h.send(serde_json::json!({
        "target_agent_id": "sender-agent",
        "message": "anyone out there?"
    }))?;

    assert!(!parsed["ok"].as_bool().unwrap());
    assert_eq!(parsed["status"].as_str().unwrap(), "no_live_recipients");
    assert_eq!(parsed["recipients_count"].as_u64().unwrap(), 0);

    Ok(())
}

/// A role whose every session has closed has no recipients.
#[serial_test::serial]
#[tokio::test]
async fn broadcast_to_fully_finished_role_reports_no_live_recipients() -> anyhow::Result<()> {
    let h = Harness::new("\"*\"")?;
    bind_session(&h.store, "old-session-1", "receiver-agent")?;
    finish_session(&h.store, "old-session-1", "receiver-agent")?;

    let parsed = h.send(serde_json::json!({
        "target_agent_id": "receiver-agent",
        "message": "hello?"
    }))?;

    assert!(!parsed["ok"].as_bool().unwrap());
    assert_eq!(parsed["status"].as_str().unwrap(), "no_live_recipients");
    assert_eq!(parsed["exists"].as_bool().unwrap(), true);
    assert_eq!(h.queued_for("old-session-1")?, 0);

    Ok(())
}

/// P-11.5 must be enforced on the *delivery* target. Addressing a session
/// directly used to skip the ACL entirely, so a grant scoped to one role could
/// message any session on the node.
#[serial_test::serial]
#[tokio::test]
async fn narrow_pattern_cannot_reach_an_arbitrary_session_by_id() -> anyhow::Result<()> {
    let h = Harness::new("\"watchdog.\"")?;
    bind_session(&h.store, "receiver-session-2", "receiver-agent")?;

    let err = h
        .send(serde_json::json!({
            "target_session_id": "receiver-session-2",
            "message": "should be denied"
        }))
        .expect_err("messaging a session outside the granted patterns must be denied");

    assert!(
        err.to_string().contains("receiver-agent"),
        "denial should name the resolved receiving agent: {err}"
    );
    assert_eq!(
        h.queued_for("receiver-session-2")?,
        0,
        "a denied send must not queue a delivery"
    );

    Ok(())
}

/// Naming a permitted role alongside an arbitrary session must not launder the
/// grant: `target_session_id` wins for delivery, so it also decides the ACL.
#[serial_test::serial]
#[tokio::test]
async fn permitted_target_agent_id_does_not_launder_a_forbidden_session_id() -> anyhow::Result<()> {
    let h = Harness::new("\"watchdog.\"")?;
    bind_session(&h.store, "receiver-session-2", "receiver-agent")?;
    bind_session(&h.store, "watchdog-session", "watchdog.default")?;

    let err = h
        .send(serde_json::json!({
            "target_agent_id": "watchdog.default",
            "target_session_id": "receiver-session-2",
            "message": "allowed role, forbidden session"
        }))
        .expect_err("the ACL must follow the session that actually receives the message");

    assert!(err.to_string().contains("receiver-agent"), "{err}");
    assert_eq!(h.queued_for("receiver-session-2")?, 0);

    Ok(())
}

/// The positive counterpart: a scoped grant still reaches sessions of a role it
/// covers.
#[serial_test::serial]
#[tokio::test]
async fn scoped_pattern_reaches_a_session_of_a_permitted_agent() -> anyhow::Result<()> {
    let h = Harness::new("\"receiver-\"")?;
    bind_session(&h.store, "receiver-session-2", "receiver-agent")?;

    let parsed = h.send(serde_json::json!({
        "target_session_id": "receiver-session-2",
        "message": "within scope"
    }))?;

    assert!(parsed["ok"].as_bool().unwrap(), "{parsed}");
    assert_eq!(parsed["recipients_count"].as_u64().unwrap(), 1);
    assert_eq!(h.queued_for("receiver-session-2")?, 1);

    Ok(())
}

/// An unknown session id has no binding, so the gateway cannot tell which
/// agent owns it and must refuse rather than deliver unchecked.
#[serial_test::serial]
#[tokio::test]
async fn unknown_session_id_is_rejected_without_delivery() -> anyhow::Result<()> {
    let h = Harness::new("\"*\"")?;

    let parsed = h.send(serde_json::json!({
        "target_session_id": "no-such-session",
        "message": "into the void"
    }))?;

    assert!(!parsed["ok"].as_bool().unwrap());
    assert_eq!(parsed["status"].as_str().unwrap(), "target_session_not_found");
    assert_eq!(parsed["recipients_count"].as_u64().unwrap(), 0);
    assert_eq!(h.queued_for("no-such-session")?, 0);

    Ok(())
}

#[serial_test::serial]
#[tokio::test]
async fn test_agent_message_missing_target_agent_returns_structured_error() -> anyhow::Result<()> {
    let h = Harness::new("\"*\"")?;

    let parsed = h.send(serde_json::json!({
        "target_agent_id": "missing-agent",
        "message": "Hello from sender"
    }))?;

    assert!(!parsed["ok"].as_bool().unwrap());
    assert_eq!(parsed["status"].as_str().unwrap(), "target_agent_not_found");
    assert_eq!(parsed["target_agent_id"].as_str().unwrap(), "missing-agent");
    assert_eq!(parsed["recipients_count"].as_u64().unwrap(), 0);
    assert_eq!(parsed["exists"].as_bool().unwrap(), false);

    Ok(())
}

#[serial_test::serial]
#[tokio::test]
async fn test_agent_message_existing_agent_without_live_session_returns_structured_error(
) -> anyhow::Result<()> {
    let h = Harness::new("\"*\"")?;

    let parsed = h.send(serde_json::json!({
        "target_agent_id": "receiver-agent",
        "message": "Hello from sender"
    }))?;

    assert!(!parsed["ok"].as_bool().unwrap());
    assert_eq!(parsed["status"].as_str().unwrap(), "no_live_recipients");
    assert_eq!(parsed["target_agent_id"].as_str().unwrap(), "receiver-agent");
    assert_eq!(parsed["recipients_count"].as_u64().unwrap(), 0);
    assert_eq!(parsed["exists"].as_bool().unwrap(), true);

    Ok(())
}

/// An installed-but-unloadable agent is a distinct failure from a missing one:
/// `target_agent_unavailable` with `exists: true`. Documented in the guidance and
/// `docs/agent-messaging.md`, so it needs coverage — a broken bundle must not be
/// reported as "not installed".
#[serial_test::serial]
#[tokio::test]
async fn broken_target_agent_bundle_reports_target_agent_unavailable() -> anyhow::Result<()> {
    let h = Harness::new("\"*\"")?;

    // Present on disk, but the frontmatter does not parse into a manifest.
    let broken = h.agents_dir.join("broken-agent");
    std::fs::create_dir_all(&broken)?;
    std::fs::write(broken.join("runtime.lock"), "dependencies: []")?;
    std::fs::write(
        broken.join("SKILL.md"),
        "---\nthis: [is, not, a, valid, manifest\n---\n# broken\n",
    )?;

    let parsed = h.send(serde_json::json!({
        "target_agent_id": "broken-agent",
        "message": "hello"
    }))?;

    assert!(!parsed["ok"].as_bool().unwrap());
    assert_eq!(
        parsed["status"].as_str().unwrap(),
        "target_agent_unavailable",
        "a broken bundle must not be reported as not-installed: {parsed}"
    );
    assert_eq!(
        parsed["exists"].as_bool().unwrap(),
        true,
        "the agent is present on disk, just unloadable: {parsed}"
    );
    assert_eq!(parsed["recipients_count"].as_u64().unwrap(), 0);

    Ok(())
}

/// Failure messages must name the tool as agents invoke it (`agent_message`), so
/// operators and agents can correlate a failure with the right tool.
#[serial_test::serial]
#[tokio::test]
async fn failure_messages_use_the_real_tool_name() -> anyhow::Result<()> {
    let h = Harness::new("\"*\"")?;

    let parsed = h.send(serde_json::json!({
        "target_agent_id": "missing-agent",
        "message": "hello"
    }))?;

    let message = parsed["message"].as_str().unwrap();
    assert!(
        message.contains("agent_message"),
        "message should name the tool: {message}"
    );
    assert!(
        !message.contains("agent.message"),
        "'agent.message' is not a tool name: {message}"
    );

    Ok(())
}

#[serial_test::serial]
#[tokio::test]
async fn either_target_is_required() -> anyhow::Result<()> {
    let h = Harness::new("\"*\"")?;

    let err = h
        .send(serde_json::json!({ "message": "no target" }))
        .expect_err("a message with no target must be rejected");
    assert!(
        err.to_string().contains("target_session_id"),
        "error should name the missing parameters: {err}"
    );

    Ok(())
}
