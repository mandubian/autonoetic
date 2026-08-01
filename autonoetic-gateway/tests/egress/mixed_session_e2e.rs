//! §5.6 mixed-session acceptance e2e (RFC data-envelopes, #907 slice 6).
//!
//! Walks the email scenario end to end at the integration boundary the prior
//! slices already expose — labeler + routing helpers + chokepoint wrapper +
//! per-band `compress_context` + a real `GatewayStore` — **including a
//! context-governor-shaped compression fire**. Does not spin a full
//! `AgentExecutor` (that would require live LLMs); each step drives the same
//! mechanical substrate the lifecycle calls.
//!
//! Steps covered:
//! 1. Operator source rules (`sandbox.exec:~/mail/** → local_only`)
//! 2. Clean turn → remote preset eligible
//! 3. `sandbox.exec` reading `~/mail/**` → labeled envelope + causal event
//! 4. Tainted batch → only local preset eligible (`egress.provider_selected`)
//! 5. Clean turn again → remote; chokepoint withholds canary from wire body
//! 7. Governor fire → per-band compression (two labeled blocks; tainted never remote)
//! 8. Causal chain answers "why this provider?" + withheld provenance
//!
//! Step 6 (`post_session_digest` / memory labels) is Phase 3 (#908) — asserted
//! only insofar as the session's accumulated taint is `local_only` (the input
//! the digest path will honor once memory labeling lands).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use autonoetic_gateway::llm::egress_chokepoint::EgressChokepointDriver;
use autonoetic_gateway::llm::{
    CompletionRequest, CompletionResponse, LlmDriver, Message, Role, StopReason, TokenUsage,
};
use autonoetic_gateway::runtime::compression::compress_context;
use autonoetic_gateway::runtime::egress_labeler::{
    emit_provider_selected, plan_taint_following_route, session_accumulated_taint, EgressLabeler,
    LabelRequest, PresetCandidate, PriorLabeledResult,
};
use autonoetic_gateway::runtime::egress_path_matcher::ExecSourceContext;
use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_types::config::{ContextCompressionConfig, LlmPreset};
use autonoetic_types::egress::{
    EgressClass, EgressConfig, EgressLabel, EgressRule, NamedEgressLabel, Sink,
};

const CANARY: &str = "CANARY-MAILBOX-EXPORT-SECRET-7e2a";
const SESSION: &str = "sess-mixed-5-6";
const AGENT: &str = "coder.default";

fn rule(source: &str, path: Option<&str>, label: NamedEgressLabel) -> EgressRule {
    EgressRule {
        source: source.to_string(),
        path: path.map(|s| s.to_string()),
        label: label.to_label(),
    }
}

fn cand(name: &str, class: EgressClass) -> PresetCandidate {
    PresetCandidate {
        name: name.to_string(),
        egress_class: Some(class),
    }
}

fn remote_preset() -> LlmPreset {
    LlmPreset {
        provider: Some("anthropic".into()),
        model: Some("claude-sonnet".into()),
        temperature: Some(0.1),
        fallback_provider: None,
        fallback_model: None,
        chat_only: None,
        context_window_tokens: None,
        base_url: None,
        api_key_env: None,
        thinking: None,
        tier: None,
        cost: None,
        latency: None,
        routing: None,
        egress_class: Some(EgressClass::Remote),
    }
}

fn tool_msg(id: &str, content: &str) -> Message {
    Message {
        id: None,
        role: Role::Tool,
        content: content.to_string(),
        tool_calls: vec![],
        tool_call_id: Some(id.to_string()),
        reasoning_content: None,
        reasoning_details: None,
    }
}

fn user_msg(content: &str) -> Message {
    Message {
        id: None,
        role: Role::User,
        content: content.to_string(),
        tool_calls: vec![],
        tool_call_id: None,
        reasoning_content: None,
        reasoning_details: None,
    }
}

fn assistant_msg_labeled(id: &str, content: &str) -> Message {
    Message {
        id: Some(id.to_string()),
        role: Role::Assistant,
        content: content.to_string(),
        tool_calls: vec![],
        tool_call_id: None,
        reasoning_content: None,
        reasoning_details: None,
    }
}

struct CapturingDriver {
    captures: Mutex<Vec<CompletionRequest>>,
}

impl CapturingDriver {
    fn new() -> Self {
        Self {
            captures: Mutex::new(Vec::new()),
        }
    }
    fn captures(&self) -> Vec<CompletionRequest> {
        self.captures.lock().unwrap().clone()
    }
}

#[async_trait]
impl LlmDriver for CapturingDriver {
    async fn complete(&self, request: &CompletionRequest) -> anyhow::Result<CompletionResponse> {
        self.captures.lock().unwrap().push(request.clone());
        Ok(CompletionResponse {
            text: "ok".into(),
            tool_calls: vec![],
            stop_reason: StopReason::EndTurn,
            usage: TokenUsage::default(),
            reasoning_content: None,
            reasoning_details: None,
        })
    }
}

fn wire_body(req: &CompletionRequest) -> String {
    req.messages
        .iter()
        .map(|m| m.content.as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

fn egress_events(
    store: &GatewayStore,
    session_id: &str,
) -> Vec<autonoetic_types::causal_chain::CausalEventRecord> {
    store
        .search_causal_events(Some(session_id), None, 100)
        .expect("search_causal_events")
        .into_iter()
        .filter(|e| e.category == "egress")
        .collect()
}

fn actions(events: &[autonoetic_types::causal_chain::CausalEventRecord]) -> Vec<&str> {
    events.iter().map(|e| e.action.as_str()).collect()
}

#[tokio::test]
async fn rfc_5_6_mixed_session_end_to_end_including_governor_fire() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let store = Arc::new(GatewayStore::open(tmp.path())?);

    // ── Step 1: operator source rules (emails stay local) ───────────────
    let cfg = EgressConfig {
        rules: vec![
            rule("email.*", None, NamedEgressLabel::LocalOnly),
            rule("fs.read", Some("~/mail/**"), NamedEgressLabel::LocalOnly),
            rule(
                "sandbox.exec",
                Some("~/mail/**"),
                NamedEgressLabel::LocalOnly,
            ),
        ],
        ..Default::default()
    };
    let labeler = EgressLabeler::from_config(&cfg);
    let mut session_labels: HashMap<String, EgressLabel> = HashMap::new();
    let no_prior = HashMap::<String, PriorLabeledResult>::new();

    let presets = vec![
        cand("sonnet", EgressClass::Remote),
        cand("ollama", EgressClass::Local),
    ];

    // ── Step 2: clean code turn → remote ────────────────────────────────
    let clean_plan = plan_taint_following_route(
        &EgressLabel::unrestricted(),
        Some(EgressClass::Remote),
        &presets,
        None,
    );
    assert!(clean_plan.primary_eligible);
    assert!(clean_plan.reroute_to.is_none());
    // Clean batches skip provider_selected emission in lifecycle; the
    // decision itself is still "remote is fine".

    // ── Step 3: sandbox.exec reads ~/mail/** → local_only envelope ──────
    let agent_dir = tmp.path().join("agent");
    std::fs::create_dir_all(&agent_dir)?;
    std::fs::write(
        agent_dir.join("parse_mail.py"),
        "import mailbox\nmb = mailbox.mbox(\"~/mail/export.mbox\")\nprint(mb)\n",
    )?;
    let exec_ctx = ExecSourceContext {
        agent_dir: Some(&agent_dir),
        gateway_dir: Some(tmp.path()),
        session_id: Some(SESSION),
        gateway_store: Some(&store),
    };
    let labeled = labeler
        .label_tool_result(
            &LabelRequest {
                tool: "sandbox_exec",
                arguments_json: r#"{"command":"python3 parse_mail.py"}"#,
                tool_call_id: "tc_mail_export",
                artifact_id: None,
            },
            Some(&exec_ctx),
            SESSION,
            AGENT,
            Some("turn-000003"),
            Some(&store),
            &no_prior,
        )
        .expect("sandbox.exec reading ~/mail/** must label the envelope");
    assert_eq!(labeled.label, EgressLabel::local_only());
    session_labels.insert("tc_mail_export".into(), labeled.label.clone());

    let canary_content = format!("mailbox export containing {CANARY}");
    // ── Step 4: tainted batch → only local eligible ─────────────────────
    let tainted_plan = plan_taint_following_route(
        &EgressLabel::local_only(),
        Some(EgressClass::Remote),
        &presets,
        None,
    );
    assert!(!tainted_plan.primary_eligible);
    assert_eq!(
        tainted_plan.reroute_to.as_ref().map(|c| c.name.as_str()),
        Some("ollama")
    );
    emit_provider_selected(
        &store,
        SESSION,
        AGENT,
        Some("turn-000004"),
        &tainted_plan,
        Some("ollama"),
        &["sonnet-fallback".into()],
        true,
        false,
        None,
    );
    // Local summary response intersects to local_only (§4.5).
    session_labels.insert("msg_local_summary".into(), EgressLabel::local_only());

    // ── Step 5: clean turn → remote again; chokepoint withholds canary ──
    let clean_again = plan_taint_following_route(
        &EgressLabel::unrestricted(),
        Some(EgressClass::Remote),
        &presets,
        None,
    );
    assert!(clean_again.primary_eligible);
    emit_provider_selected(
        &store,
        SESSION,
        AGENT,
        Some("turn-000005"),
        &clean_again,
        Some("sonnet"),
        &[],
        false,
        false,
        None,
    );

    let inner = Arc::new(CapturingDriver::new());
    let remote = EgressChokepointDriver::new(inner.clone(), Sink::RemoteModel);
    let mut meta = HashMap::new();
    meta.insert(
        autonoetic_gateway::llm::egress_chokepoint::EGRESS_LABELS_KEY.to_string(),
        serde_json::to_value(&session_labels)?,
    );
    let remote_req = CompletionRequest {
        model: "claude-sonnet".into(),
        messages: vec![
            user_msg("Write a parser script for my mailbox export."),
            user_msg("Now add error handling to the script."),
            tool_msg("tc_mail_export", &canary_content),
            assistant_msg_labeled(
                "msg_local_summary",
                &format!("Local summary of emails mentioning {CANARY}"),
            ),
            user_msg("continue the code work"),
        ],
        tools: vec![],
        max_tokens: None,
        temperature: None,
        metadata: Some(meta),
        thinking: None,
        prompt_cache_key: None,
        system_cache_prefix_bytes: None,
    };
    remote.complete(&remote_req).await?;
    let body = wire_body(&inner.captures()[0]);
    assert!(
        !body.contains(CANARY),
        "CANARY leaked to remote on clean turn:\n{body}"
    );
    assert!(
        body.contains("[withheld:"),
        "indications missing on clean remote turn:\n{body}"
    );

    // ── Step 7: governor fire → per-band compression ────────────────────
    let mut history = vec![
        user_msg("Write a parser script for my mailbox export."),
        user_msg("clean code discussion about error handling"),
        tool_msg("tc_public", "lint passed"),
        tool_msg("tc_mail_export", &canary_content),
        assistant_msg_labeled("msg_local_summary", "local email summary"),
        user_msg("recent a"),
        user_msg("recent b"),
    ];
    session_labels.insert("tc_public".into(), EgressLabel::unrestricted());

    let mut presets_map = HashMap::new();
    presets_map.insert("sonnet".into(), remote_preset());
    let compression_cfg = ContextCompressionConfig {
        enabled: true,
        llm_preset: Some("sonnet".into()),
        provider: None,
        model: None,
        threshold_pct: 0.0,
        recent_turns_to_keep: 1,
        max_summary_tokens: 64,
        min_turns_between_compression: 1,
        max_capsule_decisions: 30,
        max_completed_tasks: 10,
    };
    let client = reqwest::Client::new();
    let result = compress_context(
        history.clone(),
        Some(128_000),
        &compression_cfg,
        None,
        &presets_map,
        &client,
        SESSION,
        7,
        None,
        &mut session_labels,
    )
    .await?;
    assert!(
        result.compressed,
        "governor-shaped compression must fire on over-threshold history"
    );
    let blocks: Vec<_> = result
        .history
        .iter()
        .filter(|m| {
            m.content.starts_with("[COMPRESSED CONTEXT")
                || m.content.starts_with("[TRUNCATED CONTEXT")
        })
        .collect();
    assert!(
        blocks.len() >= 2,
        "per-band compression must yield ≥2 labeled blocks, got {}",
        blocks.len()
    );
    let mut saw_tainted_block = false;
    for block in &blocks {
        let id = block.id.as_deref().expect("synthesized block id");
        let label = session_labels
            .get(id)
            .expect("synthesized block label in sidecar");
        if *label == EgressLabel::local_only() {
            saw_tainted_block = true;
            assert!(
                block.content.starts_with("[TRUNCATED CONTEXT"),
                "tainted band must not LLM-compress on remote preset"
            );
        }
    }
    assert!(saw_tainted_block, "expected a local_only band block");
    // Source labels survive the transform (§3.4).
    assert_eq!(
        session_labels.get("tc_mail_export"),
        Some(&EgressLabel::local_only())
    );
    history = result.history;

    // ── Step 6 stub: accumulated taint is local_only (digest input) ─────
    let accumulated = session_accumulated_taint(&session_labels);
    assert_eq!(
        accumulated,
        EgressLabel::local_only(),
        "session taint must stay local_only so a later digest cannot go remote"
    );
    // LocalAgent hole closed: accumulated taint excludes RemoteModel.
    assert!(!accumulated.allows(Sink::RemoteModel));
    assert!(accumulated.allows(Sink::LocalAgent));

    // ── Step 8: causal chain answers "why this provider?" ───────────────
    let events = egress_events(&store, SESSION);
    let acts = actions(&events);
    assert!(
        acts.iter().any(|a| *a == "egress.envelope_labeled"),
        "missing envelope_labeled: {acts:?}"
    );
    assert!(
        acts.iter().any(|a| *a == "egress.provider_selected"),
        "missing provider_selected: {acts:?}"
    );

    let provider_events: Vec<_> = events
        .iter()
        .filter(|e| e.action == "egress.provider_selected")
        .collect();
    assert!(
        provider_events.len() >= 2,
        "expected tainted + clean provider_selected events, got {}",
        provider_events.len()
    );
    let tainted_evt = provider_events
        .iter()
        .find(|e| e.turn_id.as_deref() == Some("turn-000004"))
        .expect("tainted-turn provider_selected");
    let tainted_payload: serde_json::Value =
        serde_json::from_str(tainted_evt.payload.as_deref().unwrap())?;
    assert_eq!(tainted_payload["chosen_preset"], "ollama");
    assert_eq!(tainted_payload["batch_label_name"], "local_only");
    assert_eq!(tainted_payload["rerouted"], true);

    let clean_evt = provider_events
        .iter()
        .find(|e| e.turn_id.as_deref() == Some("turn-000005"))
        .expect("clean-turn provider_selected");
    let clean_payload: serde_json::Value =
        serde_json::from_str(clean_evt.payload.as_deref().unwrap())?;
    assert_eq!(clean_payload["chosen_preset"], "sonnet");

    // Post-compression history still has no path that would leak the canary
    // to a remote sink if re-filtered.
    let post_inner = Arc::new(CapturingDriver::new());
    let post_remote = EgressChokepointDriver::new(post_inner.clone(), Sink::RemoteModel);
    let mut post_meta = HashMap::new();
    post_meta.insert(
        autonoetic_gateway::llm::egress_chokepoint::EGRESS_LABELS_KEY.to_string(),
        serde_json::to_value(&session_labels)?,
    );
    let post_req = CompletionRequest {
        model: "claude-sonnet".into(),
        messages: history,
        tools: vec![],
        max_tokens: None,
        temperature: None,
        metadata: Some(post_meta),
        thinking: None,
        prompt_cache_key: None,
        system_cache_prefix_bytes: None,
    };
    post_remote.complete(&post_req).await?;
    let post_body = wire_body(&post_inner.captures()[0]);
    assert!(
        !post_body.contains(CANARY),
        "CANARY leaked after per-band compression:\n{post_body}"
    );

    Ok(())
}
