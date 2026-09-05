//! `session.inference.get/set/clear` JSON-RPC surface — the machinery behind
//! the room TUI's `/model` command. Exercises the full router dispatch path
//! (`router.dispatch`) the way the TUI reaches it: session agent binding →
//! manifest load → preset validation → root-scoped binding upsert.
//!
//! `JsonRpcRouter::new` initializes the global constitution runtime, which
//! cannot be pointed at a second workspace in-process (see
//! `crystallization/run_for_session.rs`), so all tests share one
//! router/workspace singleton and use distinct session ids per test.

use autonoetic_gateway::router::{JsonRpcRequest, JsonRpcRouter};
use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_types::agent_revision::SessionAgentBinding;
use autonoetic_types::config::{GatewayConfig, LlmPreset};
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, OnceLock};

const AGENT_ID: &str = "planner.default";

struct Surface {
    router: JsonRpcRouter,
    store: Arc<GatewayStore>,
}

fn surface() -> &'static Surface {
    static SURFACE: OnceLock<Surface> = OnceLock::new();
    SURFACE.get_or_init(|| {
        let ws = tempfile::tempdir().expect("tempdir");
        let agents_dir = ws.path().join("agents");
        std::fs::create_dir_all(&agents_dir).expect("agents dir");
        write_bundle(&agents_dir, AGENT_ID);
        let runtime_dir = agents_dir.join(".gateway");
        std::fs::create_dir_all(&runtime_dir).expect("runtime dir");
        let mut presets: HashMap<String, LlmPreset> = HashMap::new();
        for (name, provider, model, chat_only) in [
            ("sonnet", "anthropic", "claude-sonnet-4-20250514", false),
            ("fallback", "openai", "gpt-4o", false),
            ("chatty", "openai", "gpt-4o-mini", true),
        ] {
            presets.insert(
                name.to_string(),
                LlmPreset {
                    provider: Some(provider.to_string()),
                    model: Some(model.to_string()),
                    chat_only: Some(chat_only),
                    fallback_provider: None,
                    fallback_model: None,
                    temperature: None,
                    context_window_tokens: None,
                    max_tokens: None,
                    base_url: None,
                    api_key_env: None,
                    thinking: None,
                    tier: None,
                    cost: None,
                    latency: None,
                    routing: None,
                    egress_class: None,
                    request_timeout_secs: None,
                    ttfb_timeout_secs: None,
                },
            );
        }
        let config = GatewayConfig {
            agents_dir: agents_dir.clone(),
            runtime_dir,
            llm_presets: presets,
            ..GatewayConfig::default()
        };
        let store = Arc::new(GatewayStore::open(ws.path()).expect("store open"));
        // Promote the bundle (artifact -> revision -> alias) — production
        // agents are always seeded before use, and `load_agent_manifest`
        // resolves through the alias.
        crate::support::seed_agent_revision(
            &store,
            &config,
            AGENT_ID,
            &config.agents_dir.join(AGENT_ID),
        )
        .expect("seed agent revision");
        seed_agent_binding(&store, "root-1", AGENT_ID);
        // A session bound to an agent that was never promoted (no bundle, no
        // alias) — the graceful-degradation fixture for `session.inference.get`.
        seed_agent_binding(&store, "root-unpromoted", "ghost.default");
        std::mem::forget(ws);
        Surface {
            router: JsonRpcRouter::new(config, Some(store.clone())),
            store,
        }
    })
}

fn write_bundle(agents_dir: &Path, agent_id: &str) {
    let dir = agents_dir.join(agent_id);
    std::fs::create_dir_all(&dir).expect("agent dir");
    std::fs::write(
        dir.join("SKILL.md"),
        format!(
            "---\n\
             version: \"1.0\"\n\
             runtime:\n  engine: \"autonoetic\"\n  gateway_version: \"0.1.0\"\n  sdk_version: \"0.1.0\"\n  type: \"stateful\"\n  sandbox: \"bubblewrap\"\n  runtime_lock: \"runtime.lock\"\n\
             agent:\n  id: \"{agent_id}\"\n  name: \"Test Planner\"\n  description: \"inference surface stub\"\n  singleton: false\n\
             llm_preset: sonnet\n\
             capabilities:\n  - type: \"SandboxFunctions\"\n    allowed: [\"content.\"]\n\
             ---\n# Test Planner\n"
        ),
    )
    .expect("write SKILL.md");
}

fn seed_agent_binding(store: &GatewayStore, session_id: &str, agent_id: &str) {
    let binding = SessionAgentBinding {
        session_id: session_id.to_string(),
        root_session_id: session_id.to_string(),
        alias_id: None,
        agent_id: agent_id.to_string(),
        revision_id: "rev_test".to_string(),
        runtime_lock_hash: "lock_test".to_string(),
        constitution_version: None,
        constitution_digest: None,
        home_node_id: "test-node".to_string(),
        created_at: "2026-09-05T00:00:00+00:00".to_string(),
        requested_target: agent_id.to_string(),
    };
    store
        .upsert_session_agent_binding(&binding)
        .expect("seed agent binding");
}

async fn dispatch(method: &str, params: serde_json::Value) -> serde_json::Value {
    let s = surface();
    let req = JsonRpcRequest {
        jsonrpc: "2.0".to_string(),
        id: format!("t-{method}"),
        method: method.to_string(),
        params,
        auth_token: None,
    };
    let resp = s.router.dispatch(req).await;
    assert!(
        resp.error.is_none(),
        "{method} failed: {:?}",
        resp.error.map(|e| e.message)
    );
    resp.result.expect("result")
}

async fn dispatch_err(method: &str, params: serde_json::Value) -> String {
    let s = surface();
    let req = JsonRpcRequest {
        jsonrpc: "2.0".to_string(),
        id: format!("t-{method}-err"),
        method: method.to_string(),
        params,
        auth_token: None,
    };
    let resp = s.router.dispatch(req).await;
    resp.error
        .map(|e| e.message)
        .unwrap_or_else(|| "expected error".to_string())
}

#[tokio::test]
async fn inference_get_reports_resolved_profile_and_available_presets_apply() {
    // Bare `/model`: show the active profile for the session's agent.
    let v = dispatch(
        "session.inference.get",
        serde_json::json!({ "session_id": "root-1" }),
    ).await;
    assert_eq!(v["agent_id"], AGENT_ID);
    assert_eq!(v["preset_name"], "sonnet");
    assert_eq!(v["provider"], "anthropic");
    // Server-side catalog: what the RUNNING gateway allows, sorted by name —
    // not the operator client's (possibly stale) local config file.
    assert_eq!(v["agent_load_error"], serde_json::Value::Null);
    let names: Vec<&str> = v["available_presets"]
        .as_array()
        .expect("available_presets array")
        .iter()
        .filter_map(|p| p["name"].as_str())
        .collect();
    assert_eq!(names, vec!["chatty", "fallback", "sonnet"], "{v}");

    // `/model fallback`: override the session, next resolution follows.
    let v = dispatch(
        "session.inference.set",
        serde_json::json!({
            "session_id": "root-1",
            "preset": "fallback",
            "set_by": "operator:tui",
        }),
    ).await;
    assert_eq!(v["resolved"]["provider"], "openai");
    assert_eq!(v["resolved"]["model"], "gpt-4o");

    let v = dispatch(
        "session.inference.get",
        serde_json::json!({ "session_id": "root-1" }),
    ).await;
    assert_eq!(v["session_override_preset"], "fallback");
    assert_eq!(v["provider"], "openai");
    assert_eq!(v["model"], "gpt-4o");

    // `/model clear`: removes the override.
    let v = dispatch(
        "session.inference.clear",
        serde_json::json!({ "session_id": "root-1", "set_by": "operator:tui" }),
    ).await;
    assert_eq!(v["cleared"], true);
    let v = dispatch(
        "session.inference.get",
        serde_json::json!({ "session_id": "root-1" }),
    ).await;
    assert_eq!(v["preset_name"], "sonnet");
}

#[tokio::test]
async fn inference_get_degrades_gracefully_for_unpromoted_agent() {
    // A session bound to an agent that was never promoted used to fail the
    // whole RPC ("No alias found …"), taking `/model` down with it. The get
    // must now stay ok, keep the catalog visible, and surface the reason.
    let v = dispatch(
        "session.inference.get",
        serde_json::json!({ "session_id": "root-unpromoted" }),
    ).await;
    assert_eq!(v["ok"], true, "{v}");
    let err = v["agent_load_error"].as_str().expect("agent_load_error set");
    assert!(err.contains("No alias found"), "{err}");
    assert!(v["preset_name"].is_null(), "{v}");
    let names: Vec<&str> = v["available_presets"]
        .as_array()
        .expect("available_presets still served")
        .iter()
        .filter_map(|p| p["name"].as_str())
        .collect();
    assert!(!names.is_empty(), "{v}");

    // `/model <preset>` stays strict: an unvalidatable agent cannot get an
    // override — the mechanical chat_only/tool-capability gate needs the
    // manifest, and silently skipping it would bypass the safety check.
    let err = dispatch_err(
        "session.inference.set",
        serde_json::json!({ "session_id": "root-unpromoted", "preset": "fallback" }),
    ).await;
    assert!(err.contains("cannot switch model"), "{err}");
    assert!(err.contains("No alias found"), "{err}");
}

#[tokio::test]
async fn inference_set_before_first_turn_sets_override_without_agent() {
    // A session opened but not yet bound to an agent (no turn has run):
    // `/model <preset>` must work — the override is stored config-validated
    // and applies from the first turn — without skipping the chat_only gate.
    // Get reports the unbound state as information, not an error.
    let v = dispatch(
        "session.inference.get",
        serde_json::json!({ "session_id": "root-unbound" }),
    ).await;
    assert_eq!(v["ok"], true);
    assert_eq!(v["agent_bound"], false, "{v}");
    assert!(v["agent_load_error"].is_null(), "{v}");
    assert!(v["preset_name"].is_null(), "{v}");
    assert!(!v["available_presets"].as_array().expect("catalog").is_empty());

    // Non-chat_only preset: accepted, no resolved preview (no manifest).
    let v = dispatch(
        "session.inference.set",
        serde_json::json!({ "session_id": "root-unbound", "preset": "fallback" }),
    ).await;
    assert_eq!(v["ok"], true, "{v}");
    assert!(v["resolved"].is_null(), "{v}");

    // The override is durable and survives the bind: get still reports it.
    let v = dispatch(
        "session.inference.get",
        serde_json::json!({ "session_id": "root-unbound" }),
    ).await;
    assert_eq!(v["binding"]["preset_override"], "fallback", "{v}");

    // chat_only preset: rejected — no manifest exists to prove tool safety.
    let err = dispatch_err(
        "session.inference.set",
        serde_json::json!({ "session_id": "root-unbound", "preset": "chatty" }),
    ).await;
    assert!(err.contains("chat_only"), "{err}");

    // `/model clear` works pre-binding too.
    let v = dispatch(
        "session.inference.clear",
        serde_json::json!({ "session_id": "root-unbound", "set_by": "operator:tui" }),
    ).await;
    assert_eq!(v["cleared"], true);
}

#[tokio::test]
async fn inference_set_rejects_unknown_and_chat_only_presets() {
    let err = dispatch_err(
        "session.inference.set",
        serde_json::json!({ "session_id": "root-1", "preset": "no-such-preset" }),
    ).await;
    assert!(err.contains("Unknown llm preset"), "{err}");

    let err = dispatch_err(
        "session.inference.set",
        serde_json::json!({ "session_id": "root-1", "preset": "chatty" }),
    ).await;
    assert!(err.contains("chat_only"), "{err}");
}
