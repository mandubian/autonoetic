//! Post-session digest: narrative in content store + Tier-2 memories (mock LLM).

use autonoetic_gateway::llm::{
    CompletionRequest, CompletionResponse, LlmDriver, StopReason, TokenUsage,
};
use autonoetic_gateway::policy::PolicyEngine;
use autonoetic_gateway::runtime::post_session_digest::{
    run_post_session_digest_with_driver, POST_SESSION_NARRATIVE_CONTENT_NAME,
};
use autonoetic_gateway::runtime::tools::{DigestQueryTool, NativeTool};
use autonoetic_types::agent::{AgentIdentity, AgentManifest, LlmConfig, RuntimeDeclaration};
use autonoetic_types::causal_chain::ExecutionTraceRecord;
use autonoetic_types::capability::Capability;
use autonoetic_types::memory::{MemoryObject, MemorySourceType, MemoryVisibility};
use std::sync::Arc;
use tempfile::tempdir;

fn reader_manifest(agent_id: &str) -> AgentManifest {
    AgentManifest {
        version: "1.0".to_string(),
        runtime: RuntimeDeclaration {
            engine: "autonoetic".to_string(),
            gateway_version: "0.1.0".to_string(),
            sdk_version: "0.1.0".to_string(),
            runtime_type: "stateful".to_string(),
            sandbox: "bubblewrap".to_string(),
            runtime_lock: "runtime.lock".to_string(),
        },
        agent: AgentIdentity {
            id: agent_id.to_string(),
            name: agent_id.to_string(),
            description: "digest query test reader".to_string(),
        },
        capabilities: vec![Capability::ReadAccess { scopes: vec![] }],
        llm_config: Some(LlmConfig {
            provider: "openai".to_string(),
            model: "gpt-4o-mini".to_string(),
            temperature: 0.0,
            fallback_provider: None,
            fallback_model: None,
            chat_only: false,
            context_window_tokens: None,
        }),
        limits: None,
        background: None,
        disclosure: None,
        io: None,
        middleware: None,
        execution_mode: Default::default(),
        script_entry: None,
        gateway_url: None,
        gateway_token: None,
    }
}

struct FixedJsonDigestDriver;

#[async_trait::async_trait]
impl LlmDriver for FixedJsonDigestDriver {
    async fn complete(&self, _req: &CompletionRequest) -> anyhow::Result<CompletionResponse> {
        Ok(CompletionResponse {
            text: serde_json::json!({
                "narrative": "## Summary\nSession exercised digest pipeline.\n",
                "memories": [{
                    "type": "lesson",
                    "content": "Always test digest after changes.",
                    "tags": ["type:lesson", "domain:test"],
                    "confidence": 0.9
                }]
            })
            .to_string(),
            tool_calls: vec![],
            stop_reason: StopReason::EndTurn,
            usage: TokenUsage::default(),
        })
    }
}

#[tokio::test]
async fn post_session_digest_writes_narrative_and_memories() -> anyhow::Result<()> {
    let temp = tempdir()?;
    let agents_dir = temp.path().join("agents");
    std::fs::create_dir_all(agents_dir.join("digest"))?;
    std::fs::write(
        agents_dir.join("digest/SKILL.md"),
        include_str!("../../agents/digest/SKILL.md"),
    )?;
    std::fs::write(agents_dir.join("digest/runtime.lock"), "dependencies: []\n")?;

    let gateway_dir = agents_dir.join(".gateway");
    std::fs::create_dir_all(&gateway_dir)?;
    let store = Arc::new(autonoetic_gateway::scheduler::gateway_store::GatewayStore::open(
        &gateway_dir,
    )?);

    let session_id = "digest-int-session";
    let base_dir = gateway_dir.join("sessions").join(session_id);
    std::fs::create_dir_all(&base_dir)?;
    std::fs::write(
        base_dir.join("digest.md"),
        "# Live session digest: `digest-int-session`\n\n## Turn 1\nhello\n## Turn 2\nworld\n",
    )?;

    let trace = ExecutionTraceRecord {
        trace_id: "t-digest-1".to_string(),
        event_id: None,
        agent_id: "digest.agent".to_string(),
        session_id: session_id.to_string(),
        turn_id: Some("turn-000001".to_string()),
        timestamp: chrono::Utc::now().to_rfc3339(),
        tool_name: "sandbox.exec".to_string(),
        command: Some("false".to_string()),
        exit_code: Some(1),
        stdout: None,
        stderr: Some("err".to_string()),
        duration_ms: 1,
        success: 0,
        error_type: Some("runtime".to_string()),
        error_summary: Some("command failed".to_string()),
        approval_required: None,
        approval_request_id: None,
        arguments: None,
        result: None,
    };
    store.create_execution_trace(&trace)?;

    let digest_llm = LlmConfig {
        provider: "openai".to_string(),
        model: "gpt-4o-mini".to_string(),
        temperature: 0.0,
        fallback_provider: None,
        fallback_model: None,
        chat_only: true,
        context_window_tokens: None,
    };
    let driver = FixedJsonDigestDriver;
    run_post_session_digest_with_driver(
        &gateway_dir,
        &store,
        session_id,
        "digest.agent",
        &digest_llm,
        &driver,
    )
    .await?;

    let cs = autonoetic_gateway::runtime::content_store::ContentStore::new(&gateway_dir)?;
    let narr = cs.read_by_name(session_id, POST_SESSION_NARRATIVE_CONTENT_NAME)?;
    let narr_s = String::from_utf8(narr)?;
    assert!(
        narr_s.contains("digest pipeline"),
        "narrative missing expected text: {narr_s}"
    );

    let ids = store.memory_list_ids_matching_tags(
        "digest.lesson",
        "digest.agent",
        &["type:lesson".to_string()],
        None,
        10,
    )?;
    assert_eq!(ids.len(), 1);
    let m = store
        .memory_get_unrestricted(&ids[0])?
        .expect("memory must exist");
    assert!(m.content.contains("Always test digest"));
    Ok(())
}

#[test]
fn digest_query_returns_narrative_via_content_handle() -> anyhow::Result<()> {
    let temp = tempdir()?;
    let agents_dir = temp.path().join("agents");
    std::fs::create_dir_all(&agents_dir)?;
    let gateway_dir = agents_dir.join(".gateway");
    let store = Arc::new(autonoetic_gateway::scheduler::gateway_store::GatewayStore::open(
        &gateway_dir,
    )?);

    let session_id = "digest-query-handle-session";
    let cs = autonoetic_gateway::runtime::content_store::ContentStore::new(&gateway_dir)?;
    let body = b"Narrative reachable via digest.query narrative_handle.";
    let handle = cs.write(body)?;
    cs.register_name(
        session_id,
        POST_SESSION_NARRATIVE_CONTENT_NAME,
        &handle,
    )?;

    let mut mem = MemoryObject::new(
        "mem-dq-1".to_string(),
        "digest.lesson".to_string(),
        "dq.agent".to_string(),
        "dq.agent".to_string(),
        format!("session:{session_id}:test"),
        "supporting lesson for query".to_string(),
    );
    mem.source_type = MemorySourceType::AgentWrite;
    mem.tags = vec!["type:lesson".to_string()];
    mem.visibility = MemoryVisibility::Global;
    store.memory_upsert(&mem)?;

    let agent_id = "dq.agent";
    let manifest = reader_manifest(agent_id);
    let policy = PolicyEngine::new(manifest.clone());
    let agent_dir = agents_dir.join(agent_id);
    std::fs::create_dir_all(agent_dir.join("history"))?;
    std::fs::write(agent_dir.join("runtime.lock"), "dependencies: []\n")?;

    let tool = DigestQueryTool;
    let args = serde_json::json!({
        "scope": "digest.lesson",
        "tags": ["type:lesson"],
        "session_id": session_id,
        "narrative_handle": handle,
        "limit": 10,
    });
    let out = tool.execute(
        &manifest,
        &policy,
        &agent_dir,
        Some(gateway_dir.as_path()),
        &serde_json::to_string(&args)?,
        Some(session_id),
        None,
        None,
        Some(store.clone()),
        None,
    )?;
    let v: serde_json::Value = serde_json::from_str(&out)?;
    assert!(v["ok"].as_bool().unwrap_or(false));
    let narr = v["narrative"].as_object().expect("narrative object");
    let text = narr["text"].as_str().expect("narrative text");
    assert!(
        text.contains("digest.query narrative_handle"),
        "unexpected narrative: {text}"
    );
    assert_eq!(v["memory_count"], 1);
    Ok(())
}
