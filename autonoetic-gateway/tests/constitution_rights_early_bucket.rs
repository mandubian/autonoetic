//! Constitution §0 Rights audit — early bucket.
//!
//! Test-only pins for rights that are already enforced under the rule
//! framing but lack dedicated rights-level tests:
//!
//! - Ri-0.2: Every agent granted ReadAccess may search published session
//!           reports and read its own causal chain / execution trace.
//! - Ri-0.7: An agent may explicitly request session termination; the
//!           gateway may not refuse.
//! - Ri-0.11: Every action is attributed to the agent on the causal chain;
//!            hash-chain integrity detects tampering; actions cannot be
//!            retroactively reattributed.

mod support;

use autonoetic_gateway::causal_chain::CausalLogger;
use autonoetic_gateway::llm::{
    CompletionRequest, CompletionResponse, LlmDriver, Message, StopReason, TokenUsage,
};
use autonoetic_gateway::log_redaction::RedactedPayload;
use autonoetic_gateway::policy::PolicyEngine;
use autonoetic_gateway::runtime::content_store;
use autonoetic_gateway::runtime::lifecycle::AgentExecutor;
use autonoetic_gateway::runtime::tools::default_registry;
use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_types::agent::{AgentIdentity, AgentManifest, LlmConfig, RuntimeDeclaration};
use autonoetic_types::capability::Capability;
use autonoetic_types::causal_chain::{EntryStatus, PublishedSessionReportRecord};
use autonoetic_types::config::GatewayConfig;
use std::path::Path;
use std::sync::Arc;
use tempfile::tempdir;

fn manifest_with(agent_id: &str, caps: Vec<Capability>) -> AgentManifest {
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
            description: "test".to_string(),
        },
        capabilities: caps,
        llm_overrides: None,
        llm_preset: None,
        llm_config: None,
        limits: None,
        background: None,
        disclosure: None,
        io: None,
        middleware: None,
        execution_mode: Default::default(),
        script_entry: None,
        script_input_mode: Default::default(),
        gateway_url: None,
        gateway_token: None,
        allowed_tool_tiers: vec![],
        agentskills_import: None,
        compression: None,
        sandbox_network: autonoetic_types::agent::SandboxNetworkPolicy::default(),
    }
}

fn setup_gateway(base: &Path) -> (std::path::PathBuf, Arc<GatewayStore>) {
    let gw_dir = base.join(".gateway");
    std::fs::create_dir_all(&gw_dir).unwrap();
    let store = Arc::new(GatewayStore::open(&gw_dir).unwrap());
    (gw_dir, store)
}

fn log_path_for(agent_dir: &Path) -> std::path::PathBuf {
    let history = agent_dir.join("history");
    std::fs::create_dir_all(&history).unwrap();
    history.join("causal_chain.jsonl")
}

// ── Ri-0.2: Causal chain and trace read ─────────────────────────

#[test]
fn ri_0_2_agent_with_read_access_can_search_own_traces() {
    let temp = tempdir().unwrap();
    let (gw_dir, store) = setup_gateway(temp.path());

    let agent_id = "coder.default";
    let full_session_id = "root/coder.default-abc";
    let root_session_id = content_store::root_session_id(full_session_id).to_string();

    store
        .upsert_published_session_report(&PublishedSessionReportRecord {
            root_session_id: root_session_id.clone(),
            report_handle: "cnt_report".to_string(),
            overview_handle: None,
            html_handle: None,
            narrative_handle: None,
            title: "Coder session report".to_string(),
            status: "completed".to_string(),
            started_at: Some(chrono::Utc::now().to_rfc3339()),
            ended_at: Some(chrono::Utc::now().to_rfc3339()),
            agent_count: 1,
            error_count: 0,
            approval_count: 0,
            search_text: "Agent executed code and reported results.".to_string(),
            generated_at: chrono::Utc::now().to_rfc3339(),
            report_version: 1,
        })
        .unwrap();

    let manifest = manifest_with(
        agent_id,
        vec![Capability::ReadAccess {
            scopes: vec!["*".to_string()],
        }],
    );
    let policy = PolicyEngine::new(manifest.clone());
    let registry = default_registry();
    let config = GatewayConfig::default();

    let args = serde_json::json!({"query": "executed code"});
    let result = registry
        .execute(
            "observability_search",
            &manifest,
            &policy,
            temp.path(),
            Some(&gw_dir),
            &args.to_string(),
            Some(&full_session_id),
            None,
            Some(&config),
            Some(store),
            None,
        )
        .expect("observability_search should succeed with ReadAccess");

    let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
    assert_eq!(parsed["ok"], true, "search should return ok=true");
    let results = parsed["results"].as_array().expect("results array");
    assert!(!results.is_empty(), "should find the published report");
}

#[test]
fn ri_0_2_agent_without_read_access_cannot_use_observability() {
    let temp = tempdir().unwrap();
    let (gw_dir, store) = setup_gateway(temp.path());

    let manifest = manifest_with("no-read-agent", vec![]);
    let policy = PolicyEngine::new(manifest.clone());
    let registry = default_registry();

    let defs = registry.available_definitions(&manifest);
    let has_obs = defs.iter().any(|d| d.name == "observability_search");
    assert!(
        !has_obs,
        "observability_search must not be available without ReadAccess"
    );

    let args = serde_json::json!({"query": "anything"});
    let response = registry
        .execute(
            "observability_search",
            &manifest,
            &policy,
            temp.path(),
            Some(&gw_dir),
            &args.to_string(),
            None,
            None,
            None,
            Some(store),
            None,
        )
        .expect("registry returns structured envelope, not Rust Err");
    let parsed: serde_json::Value =
        serde_json::from_str(&response).expect("envelope must be JSON");
    assert_eq!(
        parsed["ok"], false,
        "direct invocation must be refused without ReadAccess"
    );
    assert_eq!(
        parsed["error_type"], "permission",
        "refusal must use the permission envelope (P-5.11)"
    );
}

// ── Ri-0.7: Session termination ─────────────────────────────────

struct EndTurnDriver;

#[async_trait::async_trait]
impl LlmDriver for EndTurnDriver {
    async fn complete(&self, _req: &CompletionRequest) -> anyhow::Result<CompletionResponse> {
        Ok(CompletionResponse {
            text: "Done.".to_string(),
            tool_calls: vec![],
            reasoning_content: None,
            reasoning_details: None,
            stop_reason: StopReason::EndTurn,
            usage: TokenUsage::default(),
        })
    }
}

fn ri_0_7_manifest() -> AgentManifest {
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
            id: "ri07.tester".to_string(),
            name: "ri07.tester".to_string(),
            description: "test".to_string(),
        },
        capabilities: vec![],
        llm_overrides: None,
        llm_preset: None,
        llm_config: Some(LlmConfig {
            provider: "openai".to_string(),
            model: "gpt-4o-mini".to_string(),
            temperature: 0.0,
            fallback_provider: None,
            fallback_model: None,
            chat_only: false,
            context_window_tokens: None,
            base_url: None,
            api_key_env: None,
            routing_preset: None,
            thinking: None,
        }),
        limits: None,
        background: None,
        disclosure: None,
        io: None,
        middleware: None,
        execution_mode: Default::default(),
        script_entry: None,
        script_input_mode: Default::default(),
        gateway_url: None,
        gateway_token: None,
        allowed_tool_tiers: vec![],
        agentskills_import: None,
        compression: None,
        sandbox_network: autonoetic_types::agent::SandboxNetworkPolicy::default(),
    }
}

#[tokio::test]
async fn ri_0_7_session_close_commits_causal_event() {
    let temp = tempdir().unwrap();
    let agents_dir = temp.path().join("agents");
    let agent_dir = agents_dir.join("ri07.tester");
    std::fs::create_dir_all(agent_dir.join("history")).unwrap();
    std::fs::write(agent_dir.join("runtime.lock"), "dependencies: []\n").unwrap();
    std::fs::write(agent_dir.join("SKILL.md"), "# ri07 tester\n").unwrap();

    let gateway_dir = agents_dir.join(".gateway");
    std::fs::create_dir_all(&gateway_dir).unwrap();
    let store = Arc::new(GatewayStore::open(&gateway_dir).unwrap());

    let driver = Arc::new(EndTurnDriver);
    let session_id = "session-ri07-close";

    let mut runtime = AgentExecutor::new(
        ri_0_7_manifest(),
        "You are a test agent.".to_string(),
        driver,
        agent_dir.clone(),
        default_registry(),
        Some(store),
    )
    .with_gateway_dir(gateway_dir)
    .with_session_id(session_id);

    let mut history = vec![
        Message::system("You are a test agent.".to_string()),
        Message::user("End.".to_string()),
    ];

    let _ = runtime
        .execute_with_history(&mut history)
        .await
        .expect("execute should succeed");

    let close_result = runtime.close_session("agent_initiated");
    assert!(
        close_result.is_ok(),
        "close_session must not be refused — Ri-0.7: {:?}",
        close_result
    );

    let history_dir = agent_dir.join("history");
    let entries = CausalLogger::read_all_entries(&history_dir).unwrap();
    let end_events: Vec<_> = entries
        .iter()
        .filter(|e| e.category == "session" && e.action == "end")
        .collect();
    assert!(
        !end_events.is_empty(),
        "should have at least one session.end event after close_session"
    );

    let end = &end_events[0];
    let payload = end.payload.as_ref().expect("end event should have payload");
    let reason = payload.get("reason").and_then(|r| r.as_str());
    assert!(
        reason.unwrap_or("").contains("agent_initiated"),
        "end event should record the termination reason: {:?}",
        reason
    );
    assert_eq!(
        end.actor_id, "ri07.tester",
        "end event should be attributed to the agent"
    );
}

#[tokio::test]
async fn ri_0_7_session_close_cannot_be_refused() {
    let temp = tempdir().unwrap();
    let agents_dir = temp.path().join("agents");
    let agent_dir = agents_dir.join("ri07.tester");
    std::fs::create_dir_all(agent_dir.join("history")).unwrap();
    std::fs::write(agent_dir.join("runtime.lock"), "dependencies: []\n").unwrap();
    std::fs::write(agent_dir.join("SKILL.md"), "# ri07 tester\n").unwrap();

    let gateway_dir = agents_dir.join(".gateway");
    std::fs::create_dir_all(&gateway_dir).unwrap();
    let store = Arc::new(GatewayStore::open(&gateway_dir).unwrap());

    let driver = Arc::new(EndTurnDriver);
    let session_id = "session-ri07-refuse";

    let mut runtime = AgentExecutor::new(
        ri_0_7_manifest(),
        "You are a test agent.".to_string(),
        driver,
        agent_dir,
        default_registry(),
        Some(store),
    )
    .with_gateway_dir(gateway_dir)
    .with_session_id(session_id);

    let mut history = vec![
        Message::system("You are a test agent.".to_string()),
        Message::user("End.".to_string()),
    ];

    let _ = runtime
        .execute_with_history(&mut history)
        .await
        .expect("execute should succeed");

    let result = runtime.close_session("agent_exit");
    assert!(
        result.is_ok(),
        "close_session must never be refused — Ri-0.7: {:?}",
        result
    );
}

// ── Ri-0.11: Non-repudiation ─────────────────────────────────────

#[test]
fn ri_0_11_every_event_carries_agent_id() {
    let temp = tempdir().unwrap();
    let agent_dir = temp.path().join("agent");
    let path = log_path_for(&agent_dir);
    let agent_id = "test-agent";
    let session_id = "root/test-agent-003";

    let logger = CausalLogger::new(&path).unwrap();

    logger
        .log(
            agent_id,
            session_id,
            None,
            0,
            "session",
            "start",
            EntryStatus::Success,
            None,
        )
        .unwrap();
    logger
        .log(
            agent_id,
            session_id,
            None,
            1,
            "tool",
            "sandbox_exec",
            EntryStatus::Success,
            Some(RedactedPayload::from_redacted(
                serde_json::json!({"cmd": "ls"}),
            )),
        )
        .unwrap();
    logger
        .log(
            agent_id,
            session_id,
            None,
            2,
            "tool",
            "content_write",
            EntryStatus::Denied,
            Some(RedactedPayload::from_redacted(
                serde_json::json!({"name": "secret"}),
            )),
        )
        .unwrap();
    logger
        .log(
            agent_id,
            session_id,
            None,
            3,
            "session",
            "end",
            EntryStatus::Success,
            None,
        )
        .unwrap();

    let entries = CausalLogger::read_entries(&path).unwrap();
    assert_eq!(entries.len(), 4, "should have 4 events");

    for (i, entry) in entries.iter().enumerate() {
        assert_eq!(
            entry.actor_id, agent_id,
            "event {} must carry agent_id '{}', got '{}'",
            i, agent_id, entry.actor_id
        );
        assert_eq!(
            entry.session_id, session_id,
            "event {} must carry correct session_id",
            i
        );
    }
}

#[test]
fn ri_0_11_hash_chain_integrity() {
    let temp = tempdir().unwrap();
    let agent_dir = temp.path().join("agent");
    let path = log_path_for(&agent_dir);
    let agent_id = "test-agent";
    let session_id = "root/test-agent-004";

    let logger = CausalLogger::new(&path).unwrap();

    logger
        .log(
            agent_id,
            session_id,
            None,
            0,
            "session",
            "start",
            EntryStatus::Success,
            None,
        )
        .unwrap();
    logger
        .log(
            agent_id,
            session_id,
            None,
            1,
            "tool",
            "sandbox_exec",
            EntryStatus::Success,
            Some(RedactedPayload::from_redacted(
                serde_json::json!({"cmd": "echo hello"}),
            )),
        )
        .unwrap();
    logger
        .log(
            agent_id,
            session_id,
            None,
            2,
            "session",
            "end",
            EntryStatus::Success,
            None,
        )
        .unwrap();

    let entries = CausalLogger::read_entries(&path).unwrap();
    assert!(
        entries.len() >= 2,
        "need at least 2 entries for chain verification"
    );

    for i in 1..entries.len() {
        assert_eq!(
            entries[i].prev_hash,
            entries[i - 1].entry_hash,
            "hash chain broken at entry {}: prev_hash {} != expected {}",
            i,
            entries[i].prev_hash,
            entries[i - 1].entry_hash
        );
    }

    for (i, entry) in entries.iter().enumerate() {
        assert!(!entry.entry_hash.is_empty(), "entry_hash must be non-empty");
        if i > 0 {
            assert_ne!(
                entry.entry_hash, entry.prev_hash,
                "entry_hash must differ from prev_hash at index {}",
                i
            );
        }
    }
}

#[test]
fn ri_0_11_tampered_actor_id_leaves_stale_hash() {
    let temp = tempdir().unwrap();
    let agent_dir = temp.path().join("agent");
    let path = log_path_for(&agent_dir);
    let real_agent = "real-agent";
    let impostor = "impostor-agent";
    let session_id = "root/test-agent-005";

    let logger = CausalLogger::new(&path).unwrap();

    logger
        .log(
            real_agent,
            session_id,
            None,
            0,
            "tool",
            "sandbox_exec",
            EntryStatus::Success,
            Some(RedactedPayload::from_redacted(
                serde_json::json!({"cmd": "ls"}),
            )),
        )
        .unwrap();

    let entries = CausalLogger::read_entries(&path).unwrap();
    let original = entries[0].clone();
    assert_eq!(original.actor_id, real_agent);

    let recomputed = autonoetic_gateway::causal_chain::compute_entry_hash(
        &original.timestamp,
        &original.log_id,
        real_agent,
        &original.session_id,
        original.turn_id.as_deref(),
        original.event_seq,
        &original.category,
        &original.action,
        &original.status,
        original.payload_hash.as_deref(),
        &original.prev_hash,
    )
    .unwrap();
    assert_eq!(
        original.entry_hash, recomputed,
        "original entry_hash must match recomputed hash"
    );

    let hash_with_impostor = autonoetic_gateway::causal_chain::compute_entry_hash(
        &original.timestamp,
        &original.log_id,
        impostor,
        &original.session_id,
        original.turn_id.as_deref(),
        original.event_seq,
        &original.category,
        &original.action,
        &original.status,
        original.payload_hash.as_deref(),
        &original.prev_hash,
    )
    .unwrap();
    assert_ne!(
        recomputed, hash_with_impostor,
        "hash with impostor agent_id must differ from original — non-repudiation"
    );

    let raw = std::fs::read_to_string(&path).unwrap();
    let tampered = raw.replace(real_agent, impostor);
    std::fs::write(&path, tampered).unwrap();

    let tampered_entries = CausalLogger::read_entries(&path).unwrap();
    let tampered_entry = &tampered_entries[0];

    assert_eq!(
        tampered_entry.actor_id, impostor,
        "tampered entry should show the impostor id"
    );
    assert_eq!(
        tampered_entry.entry_hash, original.entry_hash,
        "stored entry_hash is stale — it was computed with the real agent id, proving tampering"
    );
    assert_ne!(
        tampered_entry.entry_hash, hash_with_impostor,
        "the stored hash does NOT match what a correct hash for the impostor would be"
    );
}
