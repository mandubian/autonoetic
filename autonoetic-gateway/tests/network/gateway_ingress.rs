//! End-to-end integration test for live JSON-RPC ingress.


use crate::support::agents::install_outbound_reply_agent;
use crate::support::{
    read_causal_entries, seed_agent_revision, spawn_gateway_server_with_store, EnvGuard,
    JsonRpcClient, OpenAiStub, TestWorkspace,
};

const LLM_BASE_URL_OVERRIDE_ENV: &str = "AUTONOETIC_LLM_BASE_URL";
const LLM_API_KEY_OVERRIDE_ENV: &str = "AUTONOETIC_LLM_API_KEY";

// The test drives a live JSON-RPC ingress that triggers an outbound
// `reqwest`/`hyper` LLM call to the OpenAI stub. In debug builds (no
// inlining) the future-combinator tower from reqwest → hyper → tower's
// retry/pool layers is deep enough to overflow the default 2 MiB tokio
// worker stack; `cargo test` (debug) reliably stack-overflows here even
// though the same test passes under `--release` and at RUST_MIN_STACK=4 MiB.
//
// `#[tokio::test]` doesn't expose `thread_stack_size`, and the
// current_thread flavor runs on the calling thread anyway — so to bump
// the stack we have to spawn an OS thread with the size we want and run
// the runtime there. 8 MiB is big enough for the debug-build reqwest/
// hyper tower while not affecting any other test. This is a debug-only
// stack-depth issue, not a production bug: production runs at release
// with default stacks and never sees it.
#[test]
fn test_event_ingest_live_jsonrpc_ingress_writes_gateway_and_agent_traces(
) -> anyhow::Result<()> {
    // Spawn a worker thread with a bumped stack and run the runtime there.
    let child = std::thread::Builder::new()
        .stack_size(8 * 1024 * 1024)
        .spawn(|| {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            rt.block_on(event_ingest_live_jsonrpc_ingress_writes_gateway_and_agent_traces())
        })?;
    child.join().expect("test thread panicked")
}

async fn event_ingest_live_jsonrpc_ingress_writes_gateway_and_agent_traces(
) -> anyhow::Result<()> {
    let workspace = TestWorkspace::new()?;
    let agents_dir = workspace.agents_dir.clone();
    let target_agent_id = "agent_ingress_test";
    install_outbound_reply_agent(&agents_dir.join(target_agent_id), target_agent_id)?;

    let stub = OpenAiStub::spawn(|_, _| async move {
        serde_json::json!({
            "choices": [{
                "message": { "content": "stub assistant reply" },
                "finish_reason": "stop"
            }],
            "usage": {
                "prompt_tokens": 12,
                "completion_tokens": 3
            }
        })
    })
    .await?;
    let _base_url = EnvGuard::set(LLM_BASE_URL_OVERRIDE_ENV, stub.completion_url());
    let _api_key = EnvGuard::set(LLM_API_KEY_OVERRIDE_ENV, "test-key");

    let config = autonoetic_types::config::GatewayConfig {
        agents_dir: agents_dir.clone(),
        ..workspace.gateway_config()
    };
    let (jsonrpc_addr, store, server) = spawn_gateway_server_with_store(config.clone()).await?;

    // Seed the agent as a revision + alias
    let agent_dir = agents_dir.join(target_agent_id);
    let revision_id = seed_agent_revision(&store, &config, target_agent_id, &agent_dir)?;

    let mut client = JsonRpcClient::connect(jsonrpc_addr).await?;

    let session_id = "session-e2e-ingress";
    let response = client
        .event_ingest(
            "ingress-1",
            target_agent_id,
            session_id,
            "webhook",
            "Incoming deployment event",
            Some(serde_json::json!({"source": "integration-test"})),
        )
        .await?;

    assert!(
        response.error.is_none(),
        "unexpected error: {:?}",
        response.error
    );
    let result = response.result.expect("result should exist");
    assert_eq!(result["assistant_reply"], "stub assistant reply");
    assert_eq!(result["session_id"], session_id);
    let llm_usage = result["llm_usage"]
        .as_array()
        .expect("llm_usage should be an array");
    assert_eq!(llm_usage.len(), 1);
    assert_eq!(llm_usage[0]["input_tokens"], 12);
    assert_eq!(llm_usage[0]["output_tokens"], 3);

    let request_bodies = stub.captured_bodies();
    assert_eq!(request_bodies.len(), 1);
    let body = &request_bodies[0];
    assert_eq!(body["model"], "test-model");
    let joined_messages = body["messages"]
        .as_array()
        .expect("messages should be an array")
        .iter()
        .filter_map(|msg| msg["content"].as_str())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(joined_messages.contains("Gateway event type: webhook"));
    assert!(joined_messages.contains("Incoming deployment event"));

    // Gateway-wide causal_chain.jsonl is no longer written (events go to gateway.db).
    // This test still verifies per-session agent causal traces under the revision directory.

    let gateway_dir = agents_dir.join(".gateway");
    let rev_dir = gateway_dir
        .join("revisions")
        .join("agents")
        .join(target_agent_id)
        .join(&revision_id);
    let agent_entries = read_causal_entries(&rev_dir.join("history").join("causal_chain.jsonl"))?;

    // Per-turn correlation: agent entries should match session AND turn
    let agent_session_entries: Vec<_> = agent_entries
        .iter()
        .filter(|e| e.session_id == session_id)
        .collect();

    assert!(
        agent_session_entries
            .iter()
            .any(|entry| entry.category == "session" && entry.action == "start"),
        "Agent should have session start for {}",
        session_id
    );
    assert!(
        agent_session_entries
            .iter()
            .any(|entry| entry.category == "session" && entry.action == "end"),
        "Agent should have session end for {}",
        session_id
    );

    let turn_ids: std::collections::HashSet<_> = agent_session_entries
        .iter()
        .filter_map(|e| e.turn_id.as_ref())
        .collect();
    assert!(
        turn_ids.len() <= 1,
        "Agent entries for this session should share turn_id, found: {:?}",
        turn_ids
    );

    server.abort();
    Ok(())
}
