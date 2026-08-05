//! `approvals.approve` must return as soon as the decision is committed — the
//! post-approval session resume runs detached, in the background.
//!
//! Regression coverage for the Session Room TUI timing out after 30s when
//! approving a `credential_prompt`: the router's `approvals.approve` handler
//! used to *await* the full resumed agent turn (LLM calls, tool execution)
//! before sending the JSON-RPC response. The decision commit is fast, so the
//! response must come back well before the resumed turn finishes — and the
//! resume must still happen.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use autonoetic_gateway::router::{JsonRpcRequest, JsonRpcRouter};
use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_types::background::ScheduledAction;

use crate::support;

const AGENT_ID: &str = "cred-resume.default";
/// How long the stub LLM takes to answer the *resumed* turn's request. The
/// approve dispatch must return long before this elapses.
const RESUME_LLM_DELAY: std::time::Duration = std::time::Duration::from_secs(4);
/// Generous bound for the approve dispatch itself (decision commit only).
const APPROVE_RPC_BUDGET: std::time::Duration = std::time::Duration::from_secs(2);

fn agent_skill_md() -> String {
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
  id: cred-resume.default
  name: Cred Resume
  description: Agent that registers a credential via user_prompt
capabilities:
  - type: CredentialAccess
    services: ["github"]
llm_config:
  provider: openai
  model: test-model
  temperature: 0.0
---
# Cred Resume
"#
    .to_string()
}

fn credential_setup_call() -> serde_json::Value {
    serde_json::json!({
        "choices": [{
            "message": {
                "content": null,
                "tool_calls": [{
                    "id": "tc-cred-setup-001",
                    "type": "function",
                    "function": {
                        "name": "credential_setup",
                        "arguments": serde_json::json!({
                            "service": "github",
                            "intent": "register the github credential for later API calls",
                            "steps": [{
                                "step_type": "user_prompt",
                                "message": "Enter your GitHub token",
                                "secret_fields": [
                                    {"name": "GITHUB_TOKEN", "label": "GitHub Token", "masked": true}
                                ]
                            }]
                        }).to_string()
                    }
                }]
            },
            "finish_reason": "tool_calls"
        }],
        "usage": { "prompt_tokens": 10, "completion_tokens": 5 }
    })
}

fn final_text() -> serde_json::Value {
    serde_json::json!({
        "choices": [{
            "message": { "content": "credential registered" },
            "finish_reason": "stop"
        }],
        "usage": { "prompt_tokens": 20, "completion_tokens": 3 }
    })
}

fn make_jsonrpc(method: &str, params: serde_json::Value) -> JsonRpcRequest {
    JsonRpcRequest {
        jsonrpc: "2.0".to_string(),
        id: "approve-resume-detached".to_string(),
        method: method.to_string(),
        params,
        auth_token: None,
    }
}

// This test drives full agent turns through the router (dispatch → executor →
// reqwest/hyper LLM stub call). In debug builds the combined future depth
// overflows the default 2 MiB test-thread stack; `#[tokio::test]` doesn't
// expose `thread_stack_size`, so — same pattern as chat_ingest_routing /
// gateway_ingress (#836) — the runtime runs on an OS thread with an 8 MiB
// stack. Debug-only stack depth, not a production bug.
#[serial_test::serial]
#[test]
fn approve_returns_before_detached_resume_finishes() -> anyhow::Result<()> {
    let child = std::thread::Builder::new()
        .stack_size(8 * 1024 * 1024)
        .spawn(|| {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            rt.block_on(approve_returns_before_detached_resume_finishes_impl())
        })?;
    child.join().expect("test thread panicked")
}

async fn approve_returns_before_detached_resume_finishes_impl() -> anyhow::Result<()> {
    let workspace = support::TestWorkspace::new()?;
    let mut config = workspace.gateway_config();
    // CredentialPrompt is classified Critical (dwell + confirm phrase); zero
    // the dwell for a deterministic test.
    config.approval_dwell_multiplier = 0.0;
    let gateway_dir = workspace.agents_dir.join(".gateway");
    std::fs::create_dir_all(&gateway_dir)?;
    let store = Arc::new(GatewayStore::open(&gateway_dir)?);

    // Vault for the secret handed over at approval time.
    let vault_temp = tempfile::tempdir()?;
    std::env::set_var(
        "AUTONOETIC_VAULT_KEY",
        "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f",
    );
    std::env::set_var(
        "AUTONOETIC_VAULT_PATH",
        vault_temp.path().join("vault.enc.json"),
    );

    // Seed the agent (SKILL.md + runtime.lock + ready revision + alias).
    let agent_dir = workspace.agents_dir.join(AGENT_ID);
    std::fs::create_dir_all(&agent_dir)?;
    std::fs::write(agent_dir.join("SKILL.md"), agent_skill_md())?;
    std::fs::write(agent_dir.join("runtime.lock"), "dependencies: []\n")?;
    support::seed_agent_revision(store.as_ref(), &config, AGENT_ID, &agent_dir)?;

    // LLM stub: the first request asks for credential_setup (suspends the turn
    // on the credential_prompt approval); the resumed turn's request is answered
    // only after RESUME_LLM_DELAY.
    let calls = Arc::new(AtomicUsize::new(0));
    let calls_clone = Arc::clone(&calls);
    let stub = support::OpenAiStub::spawn(move |_raw, _body| {
        let n = calls_clone.fetch_add(1, Ordering::SeqCst);
        async move {
            if n == 0 {
                credential_setup_call()
            } else {
                tokio::time::sleep(RESUME_LLM_DELAY).await;
                final_text()
            }
        }
    })
    .await?;
    let _base_url = support::EnvGuard::set("AUTONOETIC_LLM_BASE_URL", stub.completion_url());
    let _api_key = support::EnvGuard::set("AUTONOETIC_LLM_API_KEY", "test-key");

    let router = JsonRpcRouter::new(config.clone(), Some(store.clone()));

    // Turn 1: runs to the credential_prompt suspension.
    let session_id = "approve-resume-detached-session";
    let first = router
        .spawn_agent_once(
            AGENT_ID,
            "register your github credential",
            session_id,
            None,
            false,
            None,
            None,
        )
        .await?;
    let request_id = first.suspended_for_approval.unwrap_or_else(|| {
        let captured = stub.captured_bodies();
        let tool_results: Vec<String> = captured
            .iter()
            .flat_map(|b| {
                b.get("messages")
                    .and_then(|m| m.as_array().cloned())
                    .unwrap_or_default()
            })
            .filter(|m| m.get("role").and_then(|r| r.as_str()) == Some("tool"))
            .filter_map(|m| m.get("content").and_then(|c| c.as_str()).map(String::from))
            .collect();
        panic!(
            "first turn should suspend for the credential_prompt approval; \
             reply={:?} suspended_user_input={} captured={} tool_results={:?}",
            first.assistant_reply,
            first.suspended_for_user_input,
            captured.len(),
            tool_results
        )
    });

    // The Critical-class approval needs its confirm phrase.
    let approval = store
        .get_approval(&request_id)?
        .expect("approval request should exist");
    let credential_id = match &approval.action {
        ScheduledAction::CredentialPrompt { credential_id, .. } => credential_id.clone(),
        other => panic!("expected CredentialPrompt approval, got {other:?}"),
    };
    let confirm_phrase = format!("register github {credential_id}");

    // Approve through the JSON-RPC handler. The decision commit is fast; the
    // resumed turn (stubbed to take RESUME_LLM_DELAY) must not be awaited.
    let start = std::time::Instant::now();
    let resp = router
        .dispatch(make_jsonrpc(
            "approvals.approve",
            serde_json::json!({
                "request_id": request_id,
                "decided_by": "test-operator",
                "secrets": [["GITHUB_TOKEN", "ghp_test_token_123"]],
                "confirm_phrase": confirm_phrase,
            }),
        ))
        .await;
    let elapsed = start.elapsed();

    assert!(resp.error.is_none(), "approve failed: {:?}", resp.error);
    let result = resp.result.expect("approve result");
    assert_eq!(result["status"], "Approved");
    assert!(
        elapsed < APPROVE_RPC_BUDGET,
        "approvals.approve took {elapsed:?} — the resumed turn must run detached \
         (budget {APPROVE_RPC_BUDGET:?}, stubbed resume LLM delay {RESUME_LLM_DELAY:?})"
    );

    // The detached resume still happens: the stub must see the resumed turn's
    // LLM request shortly after.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while stub.captured_bodies().len() < 2 {
        anyhow::ensure!(
            std::time::Instant::now() < deadline,
            "detached resume never reached the LLM (captured {} request(s))",
            stub.captured_bodies().len()
        );
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }

    Ok(())
}
