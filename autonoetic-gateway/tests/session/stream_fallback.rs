//! #1044: agent turns run on the streaming path; an endpoint that answers a
//! `stream: true` request with a plain JSON completion (some OpenAI-compatible
//! providers and proxies do) must degrade gracefully — parsed as a normal
//! completion, not hung or failed on the missing SSE framing.

#[tokio::test]
async fn stream_falls_back_to_json_when_endpoint_does_not_serve_sse() -> anyhow::Result<()> {
    use autonoetic_gateway::llm::{self, CompletionRequest, Message};
    use std::sync::Arc;

    let stub = crate::support::OpenAiStub::spawn(|_raw, _body| async move {
        serde_json::json!({
            "choices": [{"message": {"content": "hello"}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1}
        })
    })
    .await?;

    let resolved = llm::provider::resolve(
        "openai",
        "gpt-4o",
        None,
        None,
        Some(&stub.completion_url()),
        Some("test-key"),
        false,
        None,
        std::time::Duration::from_secs(5),
        None,
    )?;
    let driver: Arc<dyn llm::LlmDriver> =
        Arc::new(llm::openai::OpenAiDriver::new(reqwest::Client::new(), resolved));
    let req = CompletionRequest::simple("gpt-4o", vec![Message::user("hi")]);
    let ctx = llm::activity::LlmTurnCtx::detached();
    let resp = llm::complete_with_stall_detection(&driver, &req, ctx).await?;
    assert_eq!(resp.text, "hello");
    Ok(())
}
