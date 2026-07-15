//! Integration test for #719: mechanical re-execution of an approved
//! `agent_revision_promote` call on session resume.
//!
//! When a promote hits the capability-delta gate and the operator later approves,
//! the gateway must re-run the approved promote itself instead of asking the LLM
//! to re-issue the tool call. A legacy resume would need three LLM calls
//! (initial promote, re-issue after approval, final text); the mechanical path
//! needs only two.

mod support;

use autonoetic_gateway::execution::GatewayExecutionService;
use autonoetic_gateway::runtime::checkpoint::{load_latest_checkpoint, YieldReason};
use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_gateway::scheduler::{approve_request_with_options, ApproveOptions};
use autonoetic_types::agent_revision::{AgentRevisionRecord, AgentRevisionStatus};
use autonoetic_types::background::ScheduledAction;
use autonoetic_types::principal::PrincipalKind;
use autonoetic_types::promotion::PromotionRole;
use std::sync::{Arc, Mutex};

const BUILDER_AGENT_ID: &str = "builder.default";
const TARGET_AGENT_ID: &str = "target-agent";
const TARGET_REVISION_ID: &str = "rev-target-001";
const TARGET_ARTIFACT_ID: &str = "art_target_001";
const TARGET_CONTENT_DIGEST: &str = "sha256:target-digest-001";
const AUDITOR_ID: &str = "auditor.default";

fn builder_skill_md() -> String {
    r#"---
version: "1.0"
runtime:
  engine: autonoetic
  gateway_version: "0.1.0"
  sdk_version: "0.1.0"
  type: stateful
  sandbox: bubblewrap
  runtime_lock: runtime.lock
agent:
  id: builder.default
  name: Builder
  description: Builder that promotes revisions
capabilities:
  - type: AgentRevision
    patterns: ["*"]
  - type: AgentSpawn
    max_children: 10
llm_config:
  provider: openai
  model: test-model
  temperature: 0.0
---
# Builder
"#
    .to_string()
}

fn target_skill_md() -> String {
    r#"---
version: "1.0"
runtime:
  engine: autonoetic
  gateway_version: "0.1.0"
  sdk_version: "0.1.0"
  type: stateful
  sandbox: bubblewrap
  runtime_lock: runtime.lock
agent:
  id: target-agent
  name: Target Agent
  description: Target agent to promote
capabilities:
  - type: ReadAccess
    scopes: ["*"]
---
# Target
"#
    .to_string()
}

fn runtime_lock() -> String {
    "dependencies: []\n".to_string()
}

fn target_revision_record() -> AgentRevisionRecord {
    AgentRevisionRecord {
        revision_id: TARGET_REVISION_ID.to_string(),
        agent_id: TARGET_AGENT_ID.to_string(),
        base_revision_id: None,
        artifact_id: Some(TARGET_ARTIFACT_ID.to_string()),
        content_digest: TARGET_CONTENT_DIGEST.to_string(),
        runtime_lock_hash: "sha256:lock".to_string(),
        manifest_hash: "sha256:manifest".to_string(),
        created_at: chrono::Utc::now().to_rfc3339(),
        created_by_type: PrincipalKind::Human.tag().to_string(),
        created_by_id: "test-proposer".to_string(),
        requested_by_type: None,
        requested_by_id: None,
        source_kind: "test".to_string(),
        source_ref: None,
        origin_node_id: "local".to_string(),
        trust_domain: "local".to_string(),
        status: AgentRevisionStatus::Candidate,
        metadata_json: serde_json::json!({}),
        short_id: TARGET_REVISION_ID.chars().take(8).collect(),
        detected_network_hosts: None,
        signature: None,
        signer_id: None,
    }
}

fn seed_builder_agent(
    config: &autonoetic_types::config::GatewayConfig,
    store: &GatewayStore,
) -> anyhow::Result<String> {
    let agent_dir = config.agents_dir.join(BUILDER_AGENT_ID);
    std::fs::create_dir_all(&agent_dir)?;
    std::fs::write(agent_dir.join("SKILL.md"), builder_skill_md())?;
    std::fs::write(agent_dir.join("runtime.lock"), runtime_lock())?;
    support::seed_agent_revision(store, config, BUILDER_AGENT_ID, &agent_dir)
}

fn seed_target_revision(
    config: &autonoetic_types::config::GatewayConfig,
    store: &GatewayStore,
) -> anyhow::Result<()> {
    let gateway_dir = config.agents_dir.join(".gateway");
    let rev_dir = gateway_dir
        .join("revisions")
        .join("agents")
        .join(TARGET_AGENT_ID)
        .join(TARGET_REVISION_ID);
    std::fs::create_dir_all(&rev_dir)?;
    std::fs::write(rev_dir.join("SKILL.md"), target_skill_md())?;
    std::fs::write(rev_dir.join("runtime.lock"), runtime_lock())?;
    store.insert_agent_revision(&target_revision_record())?;
    Ok(())
}

fn seed_auditor_pass(gateway_dir: &std::path::Path) -> anyhow::Result<()> {
    let promo_store = autonoetic_gateway::runtime::promotion_store::PromotionStore::new(gateway_dir)?;
    promo_store.record_promotion(
        TARGET_ARTIFACT_ID.to_string(),
        None,
        Some(TARGET_CONTENT_DIGEST.to_string()),
        PromotionRole::Auditor,
        AUDITOR_ID,
        true,
        vec![],
        Some("auditor pass".to_string()),
        None,
    )?;
    Ok(())
}

fn make_stub_responses() -> Vec<serde_json::Value> {
    let promote_call = serde_json::json!({
        "choices": [{
            "message": {
                "content": null,
                "tool_calls": [{
                    "id": "tc-promote-001",
                    "type": "function",
                    "function": {
                        "name": "agent_revision_promote",
                        "arguments": serde_json::json!({
                            "agent_id": TARGET_AGENT_ID,
                            "revision_id": TARGET_REVISION_ID,
                            "intent": "promote target revision to active alias"
                        }).to_string()
                    }
                }]
            },
            "finish_reason": "tool_calls"
        }],
        "usage": { "prompt_tokens": 10, "completion_tokens": 5 }
    });

    let final_text = serde_json::json!({
        "choices": [{
            "message": {
                "content": "promoted successfully"
            },
            "finish_reason": "stop"
        }],
        "usage": { "prompt_tokens": 20, "completion_tokens": 3 }
    });

    vec![promote_call, final_text]
}

#[serial_test::serial]
#[tokio::test]
async fn test_promote_mechanically_re_executes_on_resume_without_llm_re_issue() -> anyhow::Result<()> {
    let workspace = support::TestWorkspace::new()?;
    let mut config = workspace.gateway_config();
    config.approval_dwell_multiplier = 0.0;
    let gateway_dir = workspace.agents_dir.join(".gateway");
    std::fs::create_dir_all(&gateway_dir)?;

    let store = Arc::new(GatewayStore::open(&gateway_dir)?);

    seed_builder_agent(&config, store.as_ref())?;
    seed_target_revision(&config, store.as_ref())?;
    seed_auditor_pass(&gateway_dir)?;

    let responses = Arc::new(Mutex::new(make_stub_responses()));
    let responses_clone = Arc::clone(&responses);
    let stub = support::OpenAiStub::spawn(move |_raw, _body| {
        let responses = Arc::clone(&responses_clone);
        async move {
            let mut queue = responses.lock().unwrap();
            if queue.is_empty() {
                panic!("unexpected extra LLM call — promote should re-execute mechanically, not ask the LLM to re-issue");
            }
            queue.remove(0)
        }
    })
    .await?;

    let _base_url = support::EnvGuard::set("AUTONOETIC_LLM_BASE_URL", stub.completion_url());
    let _api_key = support::EnvGuard::set("AUTONOETIC_LLM_API_KEY", "test-key");

    let execution = Arc::new(GatewayExecutionService::new(config.clone(), Some(store.clone())));
    let session_id = "promote-mechanical-resume-session";

    // First turn: the LLM issues a promote that hits the new-agent capability-delta gate.
    let first = execution
        .spawn_agent_once(
            BUILDER_AGENT_ID,
            "promote target-agent revision rev-target-001",
            session_id,
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

    let request_id = first
        .suspended_for_approval
        .expect("first turn should suspend for capability-delta approval");

    // Verify the enriched checkpoint carries the RevisionPromote pending action
    // so the mechanical resume path will be selected.
    let checkpoint = load_latest_checkpoint(&config, session_id)?.expect("checkpoint saved");
    assert!(
        matches!(checkpoint.yield_reason, YieldReason::ApprovalRequired { .. }),
        "yield reason should be ApprovalRequired"
    );
    let pts = checkpoint
        .pending_tool_state
        .expect("checkpoint should carry pending tool state");
    assert_eq!(pts.pending_tool_call.tool_name, "agent_revision_promote");
    let pending_action = checkpoint
        .pending_action
        .expect("checkpoint should carry pending action");
    assert!(
        matches!(pending_action, ScheduledAction::RevisionPromote { .. }),
        "pending action should be RevisionPromote so the mechanical resume path is selected"
    );

    // Operator approves and acknowledges the added ReadAccess capability.
    approve_request_with_options(
        &config,
        Some(store.as_ref()),
        &request_id,
        "test-operator",
        Some("acknowledged".to_string()),
        None,
        None,
        None,
        ApproveOptions {
            acknowledged_capabilities: vec!["ReadAccess".to_string()],
            confirm_phrase: Some("promote target-agent rev-target-001".to_string()),
            ..Default::default()
        },
    )?;

    // Resume. The gateway should execute the approved promote mechanically and
    // then call the LLM exactly once more for the final assistant text.
    let second = execution
        .spawn_agent_once(
            BUILDER_AGENT_ID,
            "continue",
            session_id,
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

    assert!(
        second.suspended_for_approval.is_none(),
        "resumed turn should complete without suspending again"
    );
    assert!(
        !second.suspended_for_user_input,
        "resumed turn should not block on user input"
    );
    assert_eq!(
        second.assistant_reply.as_deref(),
        Some("promoted successfully"),
        "final assistant reply should come from the second (and last) LLM call"
    );

    // The promote actually ran: alias now points at the target revision.
    let alias = store.resolve_alias(TARGET_AGENT_ID)?;
    assert!(alias.is_some(), "target agent should have an alias after promotion");
    let alias = alias.unwrap();
    assert_eq!(
        alias.revision_id, TARGET_REVISION_ID,
        "alias should point to the promoted revision"
    );

    // Exactly two LLM requests: initial prompt, then one final-text request after
    // the gateway mechanically executed the approved promote. A legacy resume
    // would need a third request (re-issue promote -> result -> final text).
    let captured = stub.captured_bodies();
    assert_eq!(
        captured.len(),
        2,
        "mechanical resume should call the LLM twice total, not three times"
    );

    let first_request = &captured[0];
    let first_messages = first_request
        .get("messages")
        .and_then(|v| v.as_array())
        .expect("first request has messages");
    assert!(
        !first_messages
            .iter()
            .any(|m| m.get("role").and_then(|r| r.as_str()) == Some("assistant")),
        "first request is the initial prompt; the LLM has not replied yet"
    );

    let second_request = &captured[1];
    let second_messages = second_request
        .get("messages")
        .and_then(|v| v.as_array())
        .expect("second request has messages");
    let assistant_msgs: Vec<_> = second_messages
        .iter()
        .filter(|m| m.get("role").and_then(|r| r.as_str()) == Some("assistant"))
        .collect();
    let synth_assistant = assistant_msgs
        .iter()
        .find(|m| {
            m.get("tool_calls")
                .and_then(|t| t.as_array())
                .map(|arr| !arr.is_empty())
                .unwrap_or(false)
        })
        .expect("second request should include the gateway-synthesized assistant message with the promote tool call");
    assert!(
        synth_assistant.get("tool_calls").is_some(),
        "synthesized assistant message must carry the mechanically-executed promote call"
    );
    let promote_results: Vec<_> = second_messages
        .iter()
        .filter(|m| {
            m.get("role").and_then(|r| r.as_str()) == Some("tool")
                && m.get("content")
                    .and_then(|c| c.as_str())
                    .map(|s| s.contains("\"status\""))
                    .unwrap_or(false)
        })
        .collect();
    assert_eq!(
        promote_results.len(),
        1,
        "second request should contain exactly one promote tool result"
    );
    let result_text = promote_results[0]
        .get("content")
        .and_then(|c| c.as_str())
        .unwrap();
    assert!(
        result_text.contains("\"status\":\"promoted\"") || result_text.contains("\"status\": \"promoted\""),
        "promote result should show status=promoted, got: {}",
        result_text
    );

    Ok(())
}
