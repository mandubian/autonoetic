//! Phase 2B: `user.ask` checkpoint suspend → store answer → resume.


use std::sync::{Arc, Mutex};

use autonoetic_gateway::execution::GatewayExecutionService;
use autonoetic_gateway::runtime::checkpoint::{load_latest_checkpoint, YieldReason};
use autonoetic_gateway::runtime::content_store::ContentStore;
use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_types::background::{UserInteractionAnswer, UserInteractionStatus};
use crate::support::{seed_agent_revision, EnvGuard, OpenAiStub, TestWorkspace};

const LLM_BASE_URL_ENV: &str = "AUTONOETIC_LLM_BASE_URL";
const LLM_API_KEY_ENV: &str = "AUTONOETIC_LLM_API_KEY";

fn install_ask_agent(agents_dir: &std::path::Path, agent_id: &str) -> anyhow::Result<()> {
    let agent_dir = agents_dir.join(agent_id);
    std::fs::create_dir_all(&agent_dir)?;
    std::fs::write(agent_dir.join("runtime.lock"), "dependencies: []")?;
    let skill = format!(
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
  description: "Asks user"
capabilities: []
llm_config:
  provider: "openai"
  model: "test-model"
  temperature: 0.0
---
# Ask agent
Use user.ask when the user wants a choice.
"#
    );
    std::fs::write(agent_dir.join("SKILL.md"), skill)?;
    Ok(())
}

fn stub_user_ask_then_text(
    ask_arguments_json: String,
    final_text: &'static str,
) -> Vec<serde_json::Value> {
    vec![
        serde_json::json!({
            "choices": [{
                "message": {
                    "content": null,
                    "tool_calls": [{
                        "id": "tc-ask-1",
                        "type": "function",
                        "function": {
                            "name": "user_ask",
                            "arguments": ask_arguments_json
                        }
                    }]
                },
                "finish_reason": "tool_calls"
            }],
            "usage": { "prompt_tokens": 5, "completion_tokens": 2 }
        }),
        serde_json::json!({
            "choices": [{
                "message": { "content": final_text },
                "finish_reason": "stop"
            }],
            "usage": { "prompt_tokens": 12, "completion_tokens": 6 }
        }),
    ]
}

#[serial_test::serial]
#[test]
fn test_user_ask_suspend_answer_resume_checkpoint() -> anyhow::Result<()> {
    // #1090: the ask/resume turn chain overflows the default 2 MiB
    // `#[tokio::test]` stack in debug builds; run on the big-stack runtime.
    crate::support::run_with_big_stack(test_user_ask_suspend_answer_resume_checkpoint_body)
}

async fn test_user_ask_suspend_answer_resume_checkpoint_body() -> anyhow::Result<()> {
    let workspace = TestWorkspace::new()?;
    let config = workspace.gateway_config();
    let gateway_dir = workspace.agents_dir.join(".gateway");
    std::fs::create_dir_all(&gateway_dir)?;

    let agent_id = "ask-agent-2b9";
    install_ask_agent(&workspace.agents_dir, agent_id)?;

    let store = Arc::new(GatewayStore::open(&gateway_dir)?);
    seed_agent_revision(
        &store,
        &config,
        agent_id,
        &workspace.agents_dir.join(agent_id),
    )?;

    let ask_args = serde_json::json!({
        "question": "Ready to proceed?",
        "kind": "clarification",
        "allow_freeform": true
    })
    .to_string();

    let responses = Arc::new(Mutex::new(stub_user_ask_then_text(
        ask_args,
        "Resumed after your answer.",
    )));
    let responses_clone = Arc::clone(&responses);

    let stub = OpenAiStub::spawn(move |_raw, _body| {
        let responses = Arc::clone(&responses_clone);
        async move {
            let mut q = responses.lock().unwrap();
            if q.is_empty() {
                serde_json::json!({
                    "choices": [{"message": {"content": "extra stub call"}, "finish_reason": "stop"}],
                    "usage": {"prompt_tokens": 1, "completion_tokens": 1}
                })
            } else {
                q.remove(0)
            }
        }
    })
    .await?;

    let _base = EnvGuard::set(LLM_BASE_URL_ENV, stub.completion_url());
    let _key = EnvGuard::set(LLM_API_KEY_ENV, "test-key");

    let execution = Arc::new(GatewayExecutionService::new(
        config.clone(),
        Some(store.clone()),
    ));

    let session_id = "session-user-ask-2b9";

    let first = execution
        .spawn_agent_once(
            agent_id,
            "Please ask if we are ready.",
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
        first.assistant_reply.is_none(),
        "user.ask should yield no assistant reply on suspend"
    );

    let cp = load_latest_checkpoint(&config, session_id)?
        .ok_or_else(|| anyhow::anyhow!("checkpoint missing after user.ask"))?;
    let interaction_id = match &cp.yield_reason {
        YieldReason::UserInputRequired { interaction_id } => interaction_id.clone(),
        other => anyhow::bail!("expected UserInputRequired, got {:?}", other),
    };

    let row = store
        .get_user_interaction(&interaction_id)?
        .ok_or_else(|| anyhow::anyhow!("interaction row missing"))?;
    assert_eq!(row.status, UserInteractionStatus::Pending);

    store.answer_user_interaction(&UserInteractionAnswer {
        interaction_id: interaction_id.clone(),
        answer_option_id: None,
        answer_text: Some("yes from test".to_string()),
        answered_by: "integration_test".to_string(),
    })?;

    let second = execution
        .spawn_agent_once(
            agent_id,
            "[operator] resume after answer",
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

    let reply = second
        .assistant_reply
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("expected assistant text after resume"))?;
    assert!(
        reply.contains("Resumed after your answer"),
        "unexpected reply: {}",
        reply
    );

    let answered = store
        .get_user_interaction(&interaction_id)?
        .expect("interaction row");
    assert_eq!(answered.status, UserInteractionStatus::Answered);

    let cp_after_resume = load_latest_checkpoint(&config, session_id)?;
    assert!(
        !matches!(
            cp_after_resume.as_ref().map(|c| &c.yield_reason),
            Some(YieldReason::UserInputRequired { .. })
        ),
        "UserInputRequired checkpoint must be consumed after resume, got {:?}",
        cp_after_resume.as_ref().map(|c| &c.yield_reason)
    );

    Ok(())
}

#[serial_test::serial]
#[test]
fn test_user_ask_resume_option_selected_value() -> anyhow::Result<()> {
    // #1090: the ask/resume turn chain overflows the default 2 MiB
    // `#[tokio::test]` stack in debug builds; run on the big-stack runtime.
    crate::support::run_with_big_stack(test_user_ask_resume_option_selected_value_body)
}

async fn test_user_ask_resume_option_selected_value_body() -> anyhow::Result<()> {
    let workspace = TestWorkspace::new()?;
    let config = workspace.gateway_config();
    let gateway_dir = workspace.agents_dir.join(".gateway");
    std::fs::create_dir_all(&gateway_dir)?;

    let agent_id = "ask-agent-2b10";
    install_ask_agent(&workspace.agents_dir, agent_id)?;

    let ask_args = serde_json::json!({
        "question": "Pick a flavor",
        "kind": "decision",
        "options": [{
            "id": "opt-vanilla",
            "label": "Vanilla",
            "value": "CANONICAL_OPTION_VAL"
        }],
        "allow_freeform": false
    })
    .to_string();

    let responses = Arc::new(Mutex::new(stub_user_ask_then_text(
        ask_args,
        "You chose CANONICAL_OPTION_VAL.",
    )));
    let responses_clone = Arc::clone(&responses);

    let stub = OpenAiStub::spawn(move |_raw, _body| {
        let responses = Arc::clone(&responses_clone);
        async move {
            let mut q = responses.lock().unwrap();
            if q.is_empty() {
                return serde_json::json!({
                    "choices": [{"message": {"content": "extra"}, "finish_reason": "stop"}],
                    "usage": {"prompt_tokens": 1, "completion_tokens": 1}
                });
            }
            q.remove(0)
        }
    })
    .await?;

    let _base = EnvGuard::set(LLM_BASE_URL_ENV, stub.completion_url());
    let _key = EnvGuard::set(LLM_API_KEY_ENV, "test-key");

    let store = Arc::new(GatewayStore::open(&gateway_dir)?);
    seed_agent_revision(
        &store,
        &config,
        agent_id,
        &workspace.agents_dir.join(agent_id),
    )?;
    let execution = Arc::new(GatewayExecutionService::new(
        config.clone(),
        Some(store.clone()),
    ));

    let session_id = "session-user-ask-2b10";

    execution
        .spawn_agent_once(
            agent_id,
            "Ask flavor with options.",
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

    let cp = load_latest_checkpoint(&config, session_id)?.expect("checkpoint");
    let interaction_id = match &cp.yield_reason {
        YieldReason::UserInputRequired { interaction_id } => interaction_id.clone(),
        other => anyhow::bail!("expected UserInputRequired, got {:?}", other),
    };

    store.answer_user_interaction(&UserInteractionAnswer {
        interaction_id,
        answer_option_id: Some("opt-vanilla".to_string()),
        answer_text: None,
        answered_by: "integration_test".to_string(),
    })?;

    let second = execution
        .spawn_agent_once(
            agent_id, "resume", session_id, None, false, None, None, None, None, None,
        &[],
        )
        .await?;

    let reply = second.assistant_reply.as_deref().unwrap_or("");
    assert!(
        reply.contains("CANONICAL_OPTION_VAL"),
        "model should see canonical option value; got {}",
        reply
    );

    Ok(())
}

#[serial_test::serial]
#[test]
fn test_user_ask_freeform_in_session_history() -> anyhow::Result<()> {
    // #1090: the ask/resume turn chain overflows the default 2 MiB
    // `#[tokio::test]` stack in debug builds; run on the big-stack runtime.
    crate::support::run_with_big_stack(test_user_ask_freeform_in_session_history_body)
}

async fn test_user_ask_freeform_in_session_history_body() -> anyhow::Result<()> {
    let workspace = TestWorkspace::new()?;
    let config = workspace.gateway_config();
    let gateway_dir = workspace.agents_dir.join(".gateway");
    std::fs::create_dir_all(&gateway_dir)?;

    let agent_id = "ask-agent-2b11";
    install_ask_agent(&workspace.agents_dir, agent_id)?;

    const MARK: &str = "FREE_FORM_USER_ANSWER_X7q";

    let ask_args = serde_json::json!({
        "question": "Any notes?",
        "kind": "clarification",
        "allow_freeform": true
    })
    .to_string();

    let responses = Arc::new(Mutex::new(stub_user_ask_then_text(
        ask_args,
        "Recorded your note.",
    )));
    let responses_clone = Arc::clone(&responses);

    let stub = OpenAiStub::spawn(move |_raw, _body| {
        let responses = Arc::clone(&responses_clone);
        async move {
            let mut q = responses.lock().unwrap();
            if q.is_empty() {
                serde_json::json!({
                    "choices": [{"message": {"content": "extra"}, "finish_reason": "stop"}],
                    "usage": {"prompt_tokens": 1, "completion_tokens": 1}
                })
            } else {
                q.remove(0)
            }
        }
    })
    .await?;

    let _base = EnvGuard::set(LLM_BASE_URL_ENV, stub.completion_url());
    let _key = EnvGuard::set(LLM_API_KEY_ENV, "test-key");

    let store = Arc::new(GatewayStore::open(&gateway_dir)?);
    seed_agent_revision(
        &store,
        &config,
        agent_id,
        &workspace.agents_dir.join(agent_id),
    )?;
    let execution = Arc::new(GatewayExecutionService::new(
        config.clone(),
        Some(store.clone()),
    ));

    let session_id = "session-user-ask-2b11";

    execution
        .spawn_agent_once(
            agent_id,
            "Ask for notes.",
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

    let cp = load_latest_checkpoint(&config, session_id)?.expect("checkpoint");
    let interaction_id = match &cp.yield_reason {
        YieldReason::UserInputRequired { interaction_id } => interaction_id.clone(),
        other => anyhow::bail!("expected UserInputRequired, got {:?}", other),
    };

    store.answer_user_interaction(&UserInteractionAnswer {
        interaction_id,
        answer_option_id: None,
        answer_text: Some(MARK.to_string()),
        answered_by: "integration_test".to_string(),
    })?;

    execution
        .spawn_agent_once(
            agent_id, "resume", session_id, None, false, None, None, None, None, None,
        &[],
        )
        .await?;

    let cs = ContentStore::new(&gateway_dir)?;
    let handle = cs
        .resolve_name_with_root(session_id, "session_history")
        .map_err(|e| anyhow::anyhow!("session_history not found: {}", e))?;
    let hist_json = cs.read_string(&handle)?;
    assert!(
        hist_json.contains(MARK),
        "freeform answer should appear in persisted session_history (digest / replay source)"
    );

    let traces = store.search_execution_traces(
        Some("user_ask"),
        None,
        None,
        None,
        Some(agent_id),
        None,
        50,
    )?;
    assert!(
        !traces.is_empty(),
        "initial user.ask dispatch should write execution_traces"
    );

    Ok(())
}

#[serial_test::serial]
#[test]
fn test_duplicate_resume_claim_guard_skips_second_caller() -> anyhow::Result<()> {
    // #1090: the ask/resume turn chain overflows the default 2 MiB
    // `#[tokio::test]` stack in debug builds; run on the big-stack runtime.
    crate::support::run_with_big_stack(test_duplicate_resume_claim_guard_skips_second_caller_body)
}

async fn test_duplicate_resume_claim_guard_skips_second_caller_body() -> anyhow::Result<()> {
    let workspace = TestWorkspace::new()?;
    let config = workspace.gateway_config();
    let gateway_dir = workspace.agents_dir.join(".gateway");
    std::fs::create_dir_all(&gateway_dir)?;

    let agent_id = "ask-agent-claim-guard";
    install_ask_agent(&workspace.agents_dir, agent_id)?;

    let ask_args = serde_json::json!({
        "question": "Proceed?",
        "kind": "clarification",
        "allow_freeform": true
    })
    .to_string();

    let call_count = Arc::new(Mutex::new(0usize));
    let call_count_clone = Arc::clone(&call_count);
    let responses = Arc::new(Mutex::new(stub_user_ask_then_text(
        ask_args,
        "Done after answer.",
    )));
    let responses_clone = Arc::clone(&responses);

    let stub = OpenAiStub::spawn(move |_raw, _body| {
        let responses = Arc::clone(&responses_clone);
        let call_count = Arc::clone(&call_count_clone);
        async move {
            *call_count.lock().unwrap() += 1;
            let mut q = responses.lock().unwrap();
            if q.is_empty() {
                serde_json::json!({
                    "choices": [{"message": {"content": "extra"}, "finish_reason": "stop"}],
                    "usage": {"prompt_tokens": 1, "completion_tokens": 1}
                })
            } else {
                q.remove(0)
            }
        }
    })
    .await?;

    let _base = EnvGuard::set(LLM_BASE_URL_ENV, stub.completion_url());
    let _key = EnvGuard::set(LLM_API_KEY_ENV, "test-key");

    let store = Arc::new(GatewayStore::open(&gateway_dir)?);
    seed_agent_revision(
        &store,
        &config,
        agent_id,
        &workspace.agents_dir.join(agent_id),
    )?;
    let execution = Arc::new(GatewayExecutionService::new(
        config.clone(),
        Some(store.clone()),
    ));

    let session_id = "session-claim-guard";

    let first = execution
        .spawn_agent_once(
            agent_id,
            "Ask if ready.",
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

    assert!(first.assistant_reply.is_none());

    let cp = load_latest_checkpoint(&config, session_id)?.expect("checkpoint");
    let interaction_id = match &cp.yield_reason {
        YieldReason::UserInputRequired { interaction_id } => interaction_id.clone(),
        other => anyhow::bail!("expected UserInputRequired, got {:?}", other),
    };

    store.answer_user_interaction(&UserInteractionAnswer {
        interaction_id: interaction_id.clone(),
        answer_option_id: None,
        answer_text: Some("yes".to_string()),
        answered_by: "integration_test".to_string(),
    })?;

    // Do NOT acquire the claim here — resume_from_user_interaction (execution.rs:2758)
    // handles it internally.  If we claimed it now, the first resume attempt would
    // fail with "already claimed".

    // The claim guard has moved to resume_from_user_interaction (execution.rs:2758),
    // which acquires the claim before spawn_agent_once → resume_from_checkpoint.
    // spawn_agent_once no longer checks the claim internally.  Instead, the gate is
    // at the resume_from_user_interaction entry: the first caller acquires the claim
    // and resumes, the second caller sees "already claimed" and bails.

    // First resume via resume_from_user_interaction — acquires claim, resumes session.
    let first_resume = execution
        .resume_from_user_interaction(
            &interaction_id,
            Some("[integration-test] resume via resume_from_user_interaction"),
        )
        .await?;
    assert!(
        first_resume.assistant_reply.is_some(),
        "first resume_from_user_interaction should succeed and produce an assistant reply"
    );

    let llm_calls_first = *call_count.lock().unwrap();
    assert_eq!(
        llm_calls_first, 2,
        "LLM should have been called twice (first spawn + resume), got {}",
        llm_calls_first
    );

    // Second resume — claim already consumed, should get "already claimed" error.
    let second_resume = execution
        .resume_from_user_interaction(
            &interaction_id,
            Some("[integration-test] second resume attempt"),
        )
        .await;
    assert!(
        second_resume.is_err(),
        "second resume_from_user_interaction should fail (claim already consumed)"
    );
    let err = second_resume.unwrap_err().to_string();
    assert!(
        err.contains("already claimed"),
        "error should mention 'already claimed', got: {}",
        err
    );

    // LLM call count should NOT have increased — the second resume never spawned.
    let llm_calls_after = *call_count.lock().unwrap();
    assert_eq!(
        llm_calls_after, 2,
        "LLM should still have been called 2 times (second resume was rejected), got {}",
        llm_calls_after
    );

    Ok(())
}
