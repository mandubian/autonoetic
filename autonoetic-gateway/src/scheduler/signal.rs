//! Signal system for inter-component communication.
//!
//! Signals are persisted as notification records in the GatewayStore (SQLite)
//! and delivered asynchronously by the scheduler's notification pump via TCP
//! JSON-RPC (`event.ingest`). There are no filesystem signal files.
//!
//! Primary use cases: approval auto-resume, workflow join wake-ups, and typed
//! child-state delivery to parent sessions.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use super::gateway_store::GatewayStore;
use autonoetic_types::notification::{NotificationRecord, NotificationType};
use autonoetic_types::workflow::ChildStateNotification;

/// Signal types that can be sent between components.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum Signal {
    /// Approval has been resolved (approved, rejected, or timed out)
    ApprovalResolved {
        request_id: String,
        agent_id: String,
        status: String, // "approved", "rejected", "timed_out"
        install_completed: bool,
        message: String,
        timestamp: String,
    },
    /// All tasks in a workflow join group have completed.
    /// Sent to the planner session so it can resume.
    WorkflowJoinSatisfied {
        workflow_id: String,
        join_task_ids: Vec<String>,
        message: String,
        /// Structured summaries of the completed child tasks, so the planner
        /// doesn't need a separate `workflow_state` or artifact inspect round.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        child_summaries: Vec<ChildStateNotification>,
        timestamp: String,
    },
    /// Typed child-state update for parent wake-up / resume.
    ChildStateNotification {
        notification: ChildStateNotification,
        message: String,
        timestamp: String,
    },
    /// A direct asynchronous message from another agent session.
    AgentMessage {
        message_id: String,
        sender_session_id: String,
        sender_agent_id: String,
        message: String,
        timestamp: String,
    },
}

/// No-op: the scheduler now handles notification polling.
pub fn start_signal_poller_if_needed(_agents_dir: PathBuf, _port: u16) -> anyhow::Result<()> {
    Ok(())
}

/// Signal file content with the filename for tracking.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingSignal {
    pub request_id: String,
    pub signal: Signal,
    pub filename: String,
}

/// Deliver a single signal via JSON-RPC event.ingest to the gateway.
pub async fn deliver_signal(
    pending: &PendingSignal,
    session_id: &str,
    port: u16,
    timeout_secs: u64,
) -> anyhow::Result<()> {
    let request_id = &pending.request_id;

    tracing::info!(
        target: "signal",
        request_id = %request_id,
        session_id = %session_id,
        "Delivering signal via JSON-RPC"
    );

    let request = build_delivery_request(pending, session_id);
    let addr = format!("127.0.0.1:{}", port);

    // Connect with retry (3 attempts)
    const MAX_ATTEMPTS: u32 = 3;
    for attempt in 1..=MAX_ATTEMPTS {
        match tokio::net::TcpStream::connect(&addr).await {
            Ok(stream) => {
                use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufWriter};
                let (read_half, write_half) = stream.into_split();
                let mut writer = BufWriter::new(write_half);
                let mut reader = tokio::io::BufReader::new(read_half);

                let encoded = serde_json::to_string(&request).unwrap_or_default();
                writer.write_all(encoded.as_bytes()).await?;
                writer.write_all(b"\n").await?;
                writer.flush().await?;

                let mut response_line = String::new();
                let read_result = tokio::time::timeout(
                    std::time::Duration::from_secs(timeout_secs.max(1)),
                    reader.read_line(&mut response_line),
                )
                .await
                .map_err(|_| anyhow::anyhow!("Timed out waiting for JSON-RPC response"))??;
                anyhow::ensure!(
                    read_result > 0,
                    "Gateway closed connection without JSON-RPC response"
                );

                let response: crate::router::JsonRpcResponse =
                    serde_json::from_str(response_line.trim())
                        .map_err(|e| anyhow::anyhow!("Invalid JSON-RPC response: {}", e))?;
                if let Some(error) = response.error {
                    anyhow::bail!("Signal delivery failed: {}", error.message);
                }
                return Ok(());
            }
            Err(_) if attempt < MAX_ATTEMPTS => {
                tokio::time::sleep(std::time::Duration::from_secs(1 << (attempt - 1))).await;
            }
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

pub fn build_delivery_request(
    pending: &PendingSignal,
    session_id: &str,
) -> crate::router::JsonRpcRequest {
    let request_id = &pending.request_id;
    let signal = &pending.signal;

    let (message, target_agent_id, signal_status, approval_request_id) = match signal {
        Signal::ApprovalResolved {
            request_id,
            agent_id,
            status,
            install_completed,
            message,
            ..
        } => (
            serde_json::json!({
                "type": "approval_resolved",
                "request_id": request_id,
                "agent_id": agent_id,
                "status": status,
                "install_completed": install_completed,
                "message": message,
            })
            .to_string(),
            Some(agent_id.clone()),
            status.clone(),
            Some(request_id.clone()),
        ),
        Signal::WorkflowJoinSatisfied {
            workflow_id,
            join_task_ids,
            message,
            child_summaries,
            ..
        } => (
            serde_json::json!({
                "type": "workflow_join_satisfied",
                "workflow_id": workflow_id,
                "join_task_ids": join_task_ids,
                "message": message,
                "child_summaries": child_summaries,
            })
            .to_string(),
            None,
            "completed".to_string(),
            None,
        ),
        Signal::ChildStateNotification {
            notification,
            message,
            ..
        } => (
            serde_json::json!({
                "type": "child_state_notification",
                "notification": notification,
                "message": message,
            })
            .to_string(),
            None,
            notification.child_status.clone(),
            None,
        ),
        // Wake notice only — deliberately does NOT carry the message body.
        //
        // The durable delivery path is the lifecycle's auto-injection, which
        // reads `agent_message_deliveries` at wake and appends the documented
        // `[Direct Message from Agent '<sender>' (Session: <sid>)]` block. This
        // ingest exists to *cause* that wake. Including the body here meant the
        // receiver saw the same text twice per message, in two different
        // formats, only one of which matches the contract the guidance teaches.
        Signal::AgentMessage {
            message_id,
            sender_session_id,
            sender_agent_id,
            ..
        } => (
            format!(
                "[Gateway] Wake-up: direct message {} from agent '{}' (session {}). \
                 Its content follows below as a `[Direct Message from Agent ...]` block; \
                 this line is only the notice that one arrived.",
                message_id, sender_agent_id, sender_session_id
            ),
            None,
            "agent_message".to_string(),
            None,
        ),
    };

    // Lets the ingress skip its `Gateway event type: ... / Message: ... /
    // Metadata: ...` envelope for signals whose text is already addressed to
    // the agent (see `raw_signal_passthrough` in `router.rs`). Child-state
    // notifications are detected by parsing their JSON payload; this wake
    // notice is prose, so it is declared explicitly instead.
    let signal_type = match signal {
        Signal::AgentMessage { .. } => Some("agent_message"),
        _ => None,
    };

    let is_async = matches!(
        signal,
        Signal::WorkflowJoinSatisfied { .. }
            | Signal::ChildStateNotification { .. }
            | Signal::AgentMessage { .. }
    );

    crate::router::JsonRpcRequest {
        jsonrpc: "2.0".to_string(),
        id: format!("signal-deliver-{}", request_id),
        method: "event.ingest".to_string(),
        params: serde_json::json!({
            "event_type": "chat",
            "target_agent_id": target_agent_id,
            "message": message,
            "session_id": session_id,
            "async_mode": is_async,
            "metadata": {
                "sender_id": "gateway-signal-poller",
                "channel_id": format!("signal-poller-{}", session_id),
                "signal_delivered": true,
                "signal_request_id": request_id,
                "signal_type": signal_type,
                "approval_request_id": approval_request_id,
                "approval_status": signal_status,
            }
        }),
        auth_token: std::env::var("AUTONOETIC_SHARED_SECRET").ok(),
    }
}

/// Write a signal to the GatewayStore as a notification.
pub fn write_signal(
    store: Option<&GatewayStore>,
    session_id: &str,
    request_id: &str,
    signal: &Signal,
) -> anyhow::Result<()> {
    let Some(store) = store else {
        return Ok(());
    };
    let n_type = match signal {
        Signal::ApprovalResolved { .. } => NotificationType::ApprovalResolved,
        Signal::WorkflowJoinSatisfied { .. } => NotificationType::WorkflowJoinSatisfied,
        Signal::ChildStateNotification { .. } => NotificationType::ChildStateNotification,
        Signal::AgentMessage { .. } => NotificationType::AgentMessage,
    };

    let n = NotificationRecord::new(
        request_id.to_string(),
        n_type,
        session_id.to_string(),
        serde_json::to_value(signal)?,
    );
    store.create_notification_record(&n)?;
    Ok(())
}
/// Write a WorkflowJoinSatisfied signal to a planner session's signal directory.
pub fn send_workflow_join_satisfied(
    store: Option<&GatewayStore>,
    root_session_id: &str,
    workflow_id: &str,
    join_task_ids: Vec<String>,
    child_summaries: Vec<ChildStateNotification>,
) -> anyhow::Result<()> {
    if workflow_id.starts_with("sched-") {
        return Ok(());
    }
    let signal_id = autonoetic_types::id_format::short_random_id("wf-join-");
    let summary_count = child_summaries.len();
    let signal = Signal::WorkflowJoinSatisfied {
        workflow_id: workflow_id.to_string(),
        join_task_ids: join_task_ids.clone(),
        message: format!(
            "Workflow join satisfied: all {} tasks completed ({} child summaries attached). You may resume planning.",
            join_task_ids.len(),
            summary_count,
        ),
        child_summaries,
        timestamp: chrono::Utc::now().to_rfc3339(),
    };
    write_signal(store, root_session_id, &signal_id, &signal)?;
    // Ring the notifier so a parent blocked in `workflow.wait` wakes via its
    // signal-driven path. The per-child notification is intentionally skipped
    // when the join is satisfied in the same update (to avoid a double wake),
    // so this is the only wake for the task that completes the join — without
    // it the parent only resumes on its 5-second fallback poll.
    if let Some(s) = store {
        s.task_notify.notify_session(root_session_id);
    }
    Ok(())
}

pub fn send_child_state_notification(
    store: Option<&GatewayStore>,
    target_session_id: &str,
    notification: ChildStateNotification,
) -> anyhow::Result<()> {
    let signal_id = autonoetic_types::id_format::short_random_id("wf-child-");
    // #1095: terminal state changes lead with the outcome (and the result
    // head) instead of a neutral "changed state to" notice — the parent must
    // not have to infer success from structured state. Non-terminal changes
    // keep the neutral wording.
    let terminal = matches!(
        notification.child_status.as_str(),
        "succeeded" | "failed" | "cancelled" | "aborted"
    );
    let message = if terminal {
        let mut msg = format!(
            "Workflow child '{}' {}.",
            notification.task_id,
            notification.child_status.to_uppercase()
        );
        if let Some(class) = notification.failure_class {
            if let Some(class) = serde_json::to_value(class)
                .ok()
                .and_then(|v| v.as_str().map(str::to_string))
            {
                msg.push_str(&format!(" Failure class: {}.", class));
            }
        }
        if let Some(ref summary) = notification.summary {
            let head: String = summary.trim().chars().take(600).collect();
            if !head.is_empty() {
                msg.push_str(&format!(" Result: {}.", head));
            }
        }
        msg
    } else {
        format!(
            "Workflow child '{}' changed state to '{}'.",
            notification.task_id, notification.child_status
        )
    };
    let signal = Signal::ChildStateNotification {
        message,
        notification,
        timestamp: chrono::Utc::now().to_rfc3339(),
    };
    write_signal(store, target_session_id, &signal_id, &signal)?;
    if let Some(s) = store {
        s.task_notify.notify_session(target_session_id);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{build_delivery_request, deliver_signal, PendingSignal, Signal};
    use autonoetic_types::tool_error::{FailureClass, RetryAdvice, SideEffectState};
    use autonoetic_types::workflow::ChildStateNotification;

    #[test]
    fn workflow_join_signal_omits_explicit_target_agent() {
        let pending = PendingSignal {
            request_id: "wf-join-test".to_string(),
            signal: Signal::WorkflowJoinSatisfied {
                workflow_id: "wf-123".to_string(),
                join_task_ids: vec!["task-a".to_string()],
                message: "ready".to_string(),
                child_summaries: Vec::new(),
                timestamp: chrono::Utc::now().to_rfc3339(),
            },
            filename: "wf-join-test.json".to_string(),
        };

        let request = build_delivery_request(&pending, "demo-session");
        assert_eq!(request.method, "event.ingest");
        assert_eq!(
            request.params.get("target_agent_id"),
            Some(&serde_json::Value::Null)
        );
        assert_eq!(
            request.params.get("async_mode"),
            Some(&serde_json::Value::Bool(true))
        );
    }

    #[test]
    fn approval_resolved_signal_is_not_async() {
        let pending = PendingSignal {
            request_id: "apr-test".to_string(),
            signal: Signal::ApprovalResolved {
                request_id: "apr-test".to_string(),
                agent_id: "coder.default".to_string(),
                status: "approved".to_string(),
                install_completed: false,
                message: "approved".to_string(),
                timestamp: chrono::Utc::now().to_rfc3339(),
            },
            filename: "apr-test.json".to_string(),
        };

        let request = build_delivery_request(&pending, "demo-session");
        match request.params.get("async_mode") {
            None | Some(serde_json::Value::Bool(false)) => {}
            other => panic!("expected async_mode=false or absent, got: {:?}", other),
        }
    }

    #[test]
    fn child_state_notification_signal_is_async_and_typed() {
        let pending = PendingSignal {
            request_id: "wf-child-test".to_string(),
            signal: Signal::ChildStateNotification {
                notification: ChildStateNotification {
                    workflow_id: "wf-123".to_string(),
                    task_id: "task-a".to_string(),
                    child_session_id: "root/task-a".to_string(),
                    child_status: "awaiting_approval".to_string(),
                    failure_class: Some(FailureClass::ApprovalPending),
                    install_conflict_detail: None,
                    retry_advice: Some(RetryAdvice::Wait),
                    side_effect_state: Some(SideEffectState::NoSideEffect),
                    agent_outcome: None,
                    summary: Some("awaiting approval apr-123".to_string()),
                },
                message: "child waiting".to_string(),
                timestamp: chrono::Utc::now().to_rfc3339(),
            },
            filename: "wf-child-test.json".to_string(),
        };

        let request = build_delivery_request(&pending, "demo-session");
        assert_eq!(
            request.params.get("async_mode"),
            Some(&serde_json::Value::Bool(true))
        );
        let metadata = request
            .params
            .get("metadata")
            .and_then(|v| v.as_object())
            .expect("metadata should be present");
        assert_eq!(
            metadata.get("signal_request_id"),
            Some(&serde_json::Value::String("wf-child-test".to_string()))
        );

        let message = request
            .params
            .get("message")
            .and_then(|v| v.as_str())
            .expect("message should be a JSON string");
        let parsed: serde_json::Value = serde_json::from_str(message).expect("message should parse");
        assert_eq!(parsed["type"], "child_state_notification");
        assert_eq!(parsed["notification"]["task_id"], "task-a");
        assert_eq!(parsed["notification"]["failure_class"], "approval_pending");
    }

    /// The wake notice must not carry the message body.
    ///
    /// The body is delivered exactly once, by the lifecycle's auto-injection of
    /// `agent_message_deliveries` as a `[Direct Message from Agent ...]` block.
    /// When this ingest also carried the body, every peer message reached the
    /// receiver twice in two different formats.
    #[test]
    fn agent_message_wake_notice_omits_the_body_and_declares_its_signal_type() {
        let pending = PendingSignal {
            request_id: "msg-abc".to_string(),
            signal: Signal::AgentMessage {
                message_id: "msg-abc".to_string(),
                sender_session_id: "sender-session-1".to_string(),
                sender_agent_id: "sender-agent".to_string(),
                message: "SENTINEL-BODY-must-not-appear-here".to_string(),
                timestamp: "2026-01-01T00:00:00Z".to_string(),
            },
            filename: "msg-abc.json".to_string(),
        };

        let request = build_delivery_request(&pending, "receiver-session-2");
        let message = request
            .params
            .get("message")
            .and_then(|v| v.as_str())
            .expect("message should be a string");
        assert!(
            !message.contains("SENTINEL-BODY-must-not-appear-here"),
            "wake notice must not duplicate the message body: {message}"
        );
        assert!(
            message.contains("msg-abc") && message.contains("sender-agent"),
            "wake notice should identify the message and its sender: {message}"
        );

        // Declaring the signal type is what lets the ingress skip its
        // `Gateway event type: ...` envelope for this prose notice.
        let metadata = request
            .params
            .get("metadata")
            .and_then(|v| v.as_object())
            .expect("metadata should be present");
        assert_eq!(
            metadata.get("signal_type"),
            Some(&serde_json::Value::String("agent_message".to_string()))
        );
        assert_eq!(
            request.params.get("async_mode"),
            Some(&serde_json::Value::Bool(true))
        );
    }

    /// Only the agent-message notice opts into raw passthrough; other signals
    /// must keep `signal_type` null so the ingress envelope still applies.
    #[test]
    fn non_agent_message_signals_declare_no_signal_type() {
        let pending = PendingSignal {
            request_id: "wf-join-test".to_string(),
            signal: Signal::WorkflowJoinSatisfied {
                workflow_id: "wf-123".to_string(),
                join_task_ids: vec!["task-a".to_string()],
                message: "ready".to_string(),
                child_summaries: Vec::new(),
                timestamp: "2026-01-01T00:00:00Z".to_string(),
            },
            filename: "wf-join-test.json".to_string(),
        };

        let request = build_delivery_request(&pending, "demo-session");
        let metadata = request
            .params
            .get("metadata")
            .and_then(|v| v.as_object())
            .expect("metadata should be present");
        assert_eq!(metadata.get("signal_type"), Some(&serde_json::Value::Null));
    }

    #[tokio::test]
    async fn deliver_signal_fails_on_jsonrpc_error_response() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener should bind");
        let port = listener
            .local_addr()
            .expect("listener should have addr")
            .port();

        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.expect("accept should succeed");
            let (read_half, mut write_half) = socket.into_split();
            let mut reader = tokio::io::BufReader::new(read_half);
            let mut line = String::new();
            let _ = tokio::io::AsyncBufReadExt::read_line(&mut reader, &mut line)
                .await
                .expect("request should read");
            let response = serde_json::json!({
                "jsonrpc": "2.0",
                "id": "signal-deliver-wf-join-test",
                "error": {
                    "code": -32000,
                    "message": "event.ingest routing failed: target_agent_id must not be empty when provided"
                }
            })
            .to_string();
            tokio::io::AsyncWriteExt::write_all(&mut write_half, response.as_bytes())
                .await
                .expect("response should write");
            tokio::io::AsyncWriteExt::write_all(&mut write_half, b"\n")
                .await
                .expect("newline should write");
        });

        let pending = PendingSignal {
            request_id: "wf-join-test".to_string(),
            signal: Signal::WorkflowJoinSatisfied {
                workflow_id: "wf-123".to_string(),
                join_task_ids: vec!["task-a".to_string()],
                message: "ready".to_string(),
                child_summaries: Vec::new(),
                timestamp: chrono::Utc::now().to_rfc3339(),
            },
            filename: "wf-join-test.json".to_string(),
        };

        let err = deliver_signal(&pending, "demo-session", port, 2)
            .await
            .expect_err("delivery should fail on JSON-RPC error");
        assert!(err
            .to_string()
            .contains("Signal delivery failed: event.ingest routing failed"));

        server.await.expect("server task should complete");
    }
}
