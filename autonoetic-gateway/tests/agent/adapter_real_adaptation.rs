//! Real-life adaptation loop: install a base agent, adapt it with the real
//! adapter scripts (schema_diff + generate_wrapper, digest captured per
//! #1227), install the wrapper through the real create+promote tools, verify
//! the roster's provenance verdict, execute the generated mapping, then
//! re-promote the base and watch `stale_base` flip and the drift event fire.
//!
//! The only mock in the room is the LLM driver — schemas, scripts, store,
//! install gates, and causal events are all real.

use autonoetic_gateway::llm::{
    CompletionRequest, CompletionResponse, LlmDriver, Message, StopReason, TokenUsage,
};
use autonoetic_gateway::policy::PolicyEngine;
use autonoetic_gateway::runtime::content_store::ContentStore;
use autonoetic_gateway::runtime::lifecycle::AgentExecutor;
use autonoetic_gateway::runtime::parser::SkillParser;
use autonoetic_gateway::runtime::tools::agent::AgentListTool;
use autonoetic_gateway::runtime::tools::{default_registry, NativeTool};
use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_types::agent::AdapterProvenance;
use autonoetic_types::capability::Capability as Cap;
use autonoetic_types::artifact::ArtifactKind;
use autonoetic_types::config::GatewayConfig;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use tempfile::tempdir;
use crate::support::manifest_builder::TestManifest;

const BASE_ID: &str = "weather.base";
const WRAPPER_ID: &str = "weather.wrapper";
const SESSION: &str = "session-real-adapt";

fn builder_manifest() -> autonoetic_types::agent::AgentManifest {
    TestManifest::new()
        .capabilities(vec![
            Cap::ReadAccess {
                scopes: vec!["self.*".to_string()],
            },
            Cap::AgentRevision {
                patterns: vec!["*".to_string()],
            },
        ])
        .agent_id("builder.test")
        .build()
}

fn is_bwrap_available() -> bool {
    Command::new("bwrap")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn script_path(rel: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("agents")
        .join("evolution")
        .join("agent-adapter.default")
        .join("scripts")
        .join(rel)
}

fn run_python_with_stdin(
    script: &Path,
    args: &[&str],
    stdin_json: &serde_json::Value,
) -> serde_json::Value {
    let mut child = Command::new("python3")
        .arg(script)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("python process should spawn");
    {
        use std::io::Write;
        let mut stdin = child.stdin.take().expect("stdin should be available");
        stdin
            .write_all(
                serde_json::to_string(stdin_json)
                    .expect("stdin json should serialize")
                    .as_bytes(),
            )
            .expect("stdin should write");
    }
    let output = child.wait_with_output().expect("python should complete");
    assert!(
        output.status.success(),
        "script failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("script stdout should be valid json")
}

/// The base contract: `{city} → {summary}`. The caller wants `{location} →
/// {result}` — a flat rename in each direction, exactly the shape the
/// mechanical layer is built for.
fn base_accepts() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "required": ["city"],
        "properties": { "city": { "type": "string" } }
    })
}

fn base_returns() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "required": ["summary"],
        "properties": { "summary": { "type": "string" } }
    })
}

fn target_spec() -> serde_json::Value {
    serde_json::json!({
        "accepts": {
            "type": "object",
            "required": ["location"],
            "properties": { "location": { "type": "string" } }
        },
        "returns": {
            "type": "object",
            "required": ["result"],
            "properties": { "result": { "type": "string" } }
        }
    })
}

struct Fixture {
    _temp: tempfile::TempDir,
    gateway_dir: PathBuf,
    agents_dir: PathBuf,
    config: GatewayConfig,
    store: Arc<GatewayStore>,
    builder: autonoetic_types::agent::AgentManifest,
    policy: PolicyEngine,
}

fn setup() -> Fixture {
    let temp = tempdir().unwrap();
    let agents_dir = temp.path().join("agents");
    std::fs::create_dir_all(&agents_dir).unwrap();
    let gateway_dir = temp.path().join(".gateway");
    std::fs::create_dir_all(&gateway_dir).unwrap();
    let store = Arc::new(GatewayStore::open(&gateway_dir).unwrap());
    let builder = builder_manifest();
    let policy = PolicyEngine::new(builder.clone());
    let config = GatewayConfig {
        runtime_dir: gateway_dir.clone(),
        agents_dir: agents_dir.clone(),
        require_operator_approval_for_new_agents: false,
        ..Default::default()
    };
    Fixture {
        _temp: temp,
        gateway_dir,
        agents_dir,
        config,
        store,
        builder,
        policy,
    }
}

fn call_tool(fx: &Fixture, name: &str, args: serde_json::Value) -> serde_json::Value {
    call_tool_session(fx, SESSION, name, args)
}

fn call_tool_session(
    fx: &Fixture,
    session: &str,
    name: &str,
    args: serde_json::Value,
) -> serde_json::Value {
    let out = default_registry()
        .execute(
            name,
            &fx.builder,
            &fx.policy,
            &fx.agents_dir.join("builder.test"),
            Some(&fx.gateway_dir),
            &serde_json::to_string(&args).unwrap(),
            Some(session),
            None,
            Some(&fx.config),
            Some(fx.store.clone()),
            None,
        )
        .unwrap_or_else(|e| panic!("{name} should execute: {e}"));
    let parsed: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(
        parsed.get("ok").and_then(|v| v.as_bool()),
        Some(true),
        "{name} should succeed: {parsed}"
    );
    parsed
}

/// Install an agent from semantic intent — the real builder flow: a small
/// artifact bundle (capability-bearing agents fail-closed without one) plus
/// the semantic install intent. Each install uses its own session so content
/// names and artifact-ref scopes don't collide.
fn install_agent(fx: &Fixture, agent_id: &str, instructions: &str, io: serde_json::Value) -> String {
    install_agent_with_replace(fx, agent_id, instructions, io, false).0
}

/// Returns `(revision_id, promote_response)` — the response lets tests assert
/// the promotion's own advisory surfaces (#1228 `adapter_drift`).
fn install_agent_with_replace(
    fx: &Fixture,
    agent_id: &str,
    instructions: &str,
    io: serde_json::Value,
    replace: bool,
) -> (String, serde_json::Value) {
    let session = format!("{SESSION}-{agent_id}");
    let content_store = ContentStore::new(&fx.gateway_dir).unwrap();
    let artifact_store =
        autonoetic_gateway::artifact_store::ArtifactStore::new(&fx.gateway_dir).unwrap();
    let runtime_lock = r#"gateway:
  artifact: autonoetic-gateway
  version: "0.1.0"
  sha256: unmanaged
  signature: null
sdk:
  version: "0.1.0"
sandbox:
  backend: bubblewrap
dependencies: []
artifacts: []
layers: []
"#;
    let mut names = Vec::new();
    for (rel, content) in [
        ("SKILL.md", instructions.as_bytes().to_vec()),
        ("runtime.lock", runtime_lock.as_bytes().to_vec()),
        ("main.py", b"#!/usr/bin/env python3\nprint('{}')\n".to_vec()),
    ] {
        let handle = content_store.write(&content).unwrap();
        content_store.register_name(&session, rel, &handle).unwrap();
        names.push(rel.to_string());
    }
    let bundle = artifact_store
        .build_with_kind(&names, None, None, ArtifactKind::AgentBundle, &session)
        .unwrap();
    // `ar.` + agent-id alphanumerics + a 4-digit install sequence — unique
    // per install (a base re-installs with a fresh artifact).
    static INSTALL_SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(1);
    let seq = INSTALL_SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let stripped: String = agent_id.chars().filter(|c| c.is_ascii_alphanumeric()).collect();
    let artifact_ref = format!(
        "ar.{}{:0>4}",
        &stripped[..stripped.len().min(8)],
        seq
    );
    fx.store
        .create_artifact_ref(&autonoetic_types::artifact::ArtifactRefRecord {
            ref_id: artifact_ref.clone(),
            scope_type: autonoetic_types::artifact::ArtifactRefScopeType::Session,
            scope_id: session.clone(),
            artifact_id: bundle.artifact_id.clone(),
            artifact_manifest_digest: bundle.artifact_manifest_digest.clone(),
            artifact_canonical_digest: bundle.artifact_canonical_digest.clone(),
            created_by_agent_id: "builder.test".to_string(),
            created_at: chrono::Utc::now().to_rfc3339(),
            expires_at: None,
            revoked_at: None,
        })
        .unwrap();

    let res = call_tool_session(
        fx,
        &session,
        "agent_revision_create_from_intent",
        serde_json::json!({
            "agent_id": agent_id,
            "description": format!("{agent_id} fixture"),
            "instructions": instructions,
            "capabilities": [
                { "type": "ReadAccess", "scopes": ["self.*"] }
            ],
            "io": io,
            "llm_preset": "agentic",
            "artifact_ref": artifact_ref,
            "replace": replace,
        }),
    );
    let revision_id = res["revision_id"].as_str().unwrap().to_string();
    // Federation gate, real shape: a pure-skill agent needs an auditor pass
    // record bound to the artifact + revision digest before promote.
    let digest_hex = revision_id.strip_prefix("rev_sha256:").unwrap_or_else(|| {
        panic!("unexpected revision id format: {revision_id}")
    });
    let content_digest = format!("sha256:{digest_hex}");
    let promo_store = autonoetic_gateway::runtime::promotion_store::PromotionStore::new(
        &fx.gateway_dir,
    )
    .unwrap();
    crate::support::promotion_trace::seed_promotion_store_execution_role(
        &promo_store,
        &fx.store,
        &bundle.artifact_id,
        autonoetic_types::promotion::PromotionRole::Auditor,
        "auditor.default",
        true,
        &session,
        Some(&content_digest),
    );
    let promote_response = call_tool_session(
        fx,
        &session,
        "agent_revision_promote",
        serde_json::json!({ "agent_id": agent_id, "revision_id": revision_id, "reason": "real-life fixture" }),
    );
    (revision_id, promote_response)
}

/// Run the real adapter scripts: schema_diff → generate_wrapper with the
/// base's promoted digest captured from the store (what #1227 instructs the
/// adapter agent to do). Returns the generated wrapper directory.
fn generate_wrapper_bundle(
    fx: &Fixture,
    base_revision_digest: &str,
) -> PathBuf {
    let diff = run_python_with_stdin(
        &script_path("schema_diff.py"),
        &[],
        &serde_json::json!({
            "base_accepts": base_accepts(),
            "base_returns": base_returns(),
            "target_accepts": target_spec()["accepts"],
            "target_returns": target_spec()["returns"],
        }),
    );
    assert_eq!(diff["requires_input_mapping"], true, "{diff}");
    assert_eq!(diff["requires_output_mapping"], true, "{diff}");

    let base_skill_file = fx._temp.path().join("base-skill.md");
    std::fs::write(
        &base_skill_file,
        format!(
            "---\nname: \"{BASE_ID}\"\ndescription: \"weather\"\n---\n# Weather Base\nFetches weather for a city.\n"
        ),
    )
    .unwrap();

    let out_dir = fx._temp.path().join("wrapper-generated");
    let out = Command::new("python3")
        .arg(script_path("generate_wrapper.py"))
        .arg("--base-skill")
        .arg(base_skill_file.to_string_lossy().to_string())
        .arg("--base-agent-id")
        .arg(BASE_ID)
        .arg("--wrapper-id")
        .arg(WRAPPER_ID)
        .arg("--target-spec-json")
        .arg(serde_json::to_string(&target_spec()).unwrap())
        .arg("--schema-diff-json")
        .arg(serde_json::to_string(&diff).unwrap())
        .arg("--base-manifest-json")
        .arg(r#"{"capabilities": [{"type": "ReadAccess", "scopes": ["self.*"]}]}"#)
        .arg("--base-revision-digest")
        .arg(base_revision_digest)
        .arg("--output-dir")
        .arg(out_dir.to_string_lossy().to_string())
        .output()
        .expect("generate_wrapper.py should execute");
    assert!(
        out.status.success(),
        "generate_wrapper.py failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    out_dir
}

fn list_agents(fx: &Fixture) -> Vec<serde_json::Value> {
    let tool = AgentListTool;
    let manifest = TestManifest::new()
        .capabilities(vec![Cap::SandboxFunctions {
            allowed: vec!["*".to_string()],
        }])
        .build();
    let policy = PolicyEngine::new(manifest.clone());
    let out = tool
        .execute(
            &manifest,
            &policy,
            &fx.agents_dir,
            Some(&fx.gateway_dir),
            "{}",
            Some(SESSION),
            None,
            Some(&fx.config),
            Some(fx.store.clone()),
            None,
        )
        .unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&out).unwrap();
    parsed["agents"].as_array().unwrap().clone()
}

fn find_agent<'a>(agents: &'a [serde_json::Value], id: &str) -> &'a serde_json::Value {
    agents
        .iter()
        .find(|a| a["agent_id"] == id)
        .unwrap_or_else(|| panic!("'{id}' should be listed, got: {agents:?}"))
}

/// The full loop minus execution: install base → adapt with the real scripts
/// → install wrapper via create_from_intent (artifact + middleware +
/// provenance) → promote → roster verdict. Then re-promote the base and watch
/// `stale_base` flip AND the promotion-time drift event fire.
#[test]
fn real_adaptation_loop_install_roster_and_drift() {
    let fx = setup();

    // 1. Base agent, installed for real.
    let base_rev_a = install_agent(
        &fx,
        BASE_ID,
        "# Weather Base\nFetches weather for a city.\n",
        serde_json::json!({ "accepts": base_accepts(), "returns": base_returns() }),
    );
    assert!(base_rev_a.starts_with("rev_sha256:"));

    // 2. Adapt it with the real scripts, digest captured from the store.
    let wrapper_dir = generate_wrapper_bundle(&fx, &base_rev_a);
    let generated_skill =
        std::fs::read_to_string(wrapper_dir.join("SKILL.md")).unwrap();
    let (_, body) = SkillParser::parse(&generated_skill).unwrap();

    // 3. Install the wrapper the way the builder does: artifact + intent.
    let content_store = ContentStore::new(&fx.gateway_dir).unwrap();
    let artifact_store =
        autonoetic_gateway::artifact_store::ArtifactStore::new(&fx.gateway_dir).unwrap();
    let mut names = Vec::new();
    for rel in [
        "SKILL.md",
        "runtime.lock",
        "scripts/pre_map.py",
        "scripts/post_map.py",
    ] {
        let content = std::fs::read(wrapper_dir.join(rel)).unwrap();
        let handle = content_store.write(&content).unwrap();
        content_store.register_name(SESSION, rel, &handle).unwrap();
        names.push(rel.to_string());
    }
    let bundle = artifact_store
        .build_with_kind(&names, None, None, ArtifactKind::AgentBundle, SESSION)
        .unwrap();
    let artifact_ref = "ar.reale2e00001";
    fx.store
        .create_artifact_ref(&autonoetic_types::artifact::ArtifactRefRecord {
            ref_id: artifact_ref.to_string(),
            scope_type: autonoetic_types::artifact::ArtifactRefScopeType::Session,
            scope_id: SESSION.to_string(),
            artifact_id: bundle.artifact_id.clone(),
            artifact_manifest_digest: bundle.artifact_manifest_digest.clone(),
            artifact_canonical_digest: bundle.artifact_canonical_digest.clone(),
            created_by_agent_id: "builder.test".to_string(),
            created_at: chrono::Utc::now().to_rfc3339(),
            expires_at: None,
            revoked_at: None,
        })
        .unwrap();

    let adapter = AdapterProvenance {
        base_agent_id: BASE_ID.to_string(),
        base_revision_digest: Some(base_rev_a.clone()),
        generated_at: Some("2026-08-29T00:00:00Z".to_string()),
        schema_notes: vec!["accepts: rename location->city".to_string()],
        generator: Some("agent-adapter.default".to_string()),
    };
    let wrapper_res = call_tool(
        &fx,
        "agent_revision_create_from_intent",
        serde_json::json!({
            "agent_id": WRAPPER_ID,
            "description": "Wrapper generated by agent-adapter.default",
            "instructions": body,
            "capabilities": [
                { "type": "ReadAccess", "scopes": ["self.*"] }
            ],
            "io": target_spec(),
            "middleware": {
                "pre_process": "python3 scripts/pre_map.py",
                "post_process": "python3 scripts/post_map.py"
            },
            "adapter": adapter,
            "artifact_ref": artifact_ref,
            "llm_preset": "agentic",
        }),
    );
    let wrapper_rev = wrapper_res["revision_id"].as_str().unwrap().to_string();
    // The wrapper is a capability-bearing pure-skill agent too: auditor pass
    // record bound to its artifact + digest, then promote.
    let wrapper_artifact_id = wrapper_res["artifact_id"].as_str().unwrap().to_string();
    let wrapper_digest_hex = wrapper_rev
        .strip_prefix("rev_sha256:")
        .unwrap_or_else(|| panic!("unexpected revision id format: {wrapper_rev}"));
    let wrapper_digest = format!("sha256:{wrapper_digest_hex}");
    let wrapper_promo_store = autonoetic_gateway::runtime::promotion_store::PromotionStore::new(
        &fx.gateway_dir,
    )
    .unwrap();
    crate::support::promotion_trace::seed_promotion_store_execution_role(
        &wrapper_promo_store,
        &fx.store,
        &wrapper_artifact_id,
        autonoetic_types::promotion::PromotionRole::Auditor,
        "auditor.default",
        true,
        SESSION,
        Some(&wrapper_digest),
    );
    call_tool(
        &fx,
        "agent_revision_promote",
        serde_json::json!({ "agent_id": WRAPPER_ID, "revision_id": wrapper_rev, "reason": "real-life fixture" }),
    );

    // 4. The installed canonical manifest keeps middleware + provenance.
    let installed_skill = std::fs::read_to_string(
        autonoetic_gateway::agent::agent_revision_dir(
            &fx.gateway_dir,
            WRAPPER_ID,
            &wrapper_rev,
        )
        .join("SKILL.md"),
    )
    .unwrap();
    let (installed_manifest, _) = SkillParser::parse(&installed_skill).unwrap();
    let installed_mw = installed_manifest
        .middleware
        .clone()
        .expect("installed wrapper keeps middleware");
    assert_eq!(
        installed_mw.pre_process.as_deref(),
        Some("python3 scripts/pre_map.py")
    );
    let installed_adapter = installed_manifest
        .adapter
        .expect("installed wrapper keeps provenance");
    assert_eq!(installed_adapter.base_agent_id, BASE_ID);
    assert_eq!(installed_adapter.base_revision_digest.as_deref(), Some(base_rev_a.as_str()));

    // 5. Roster: current against the base.
    let agents = list_agents(&fx);
    let wrapper = find_agent(&agents, WRAPPER_ID);
    assert_eq!(wrapper["adapter"]["base_agent_id"], BASE_ID);
    assert_eq!(wrapper["stale_base"], false);

    // 6. Base moves on (new revision, real create + promote → drift event).
    let (base_rev_b, base_promo_res) = install_agent_with_replace(
        &fx,
        BASE_ID,
        "# Weather Base v2\nFetches weather, now with humidity.\n",
        serde_json::json!({ "accepts": base_accepts(), "returns": base_returns() }),
        true,
    );
    assert_ne!(base_rev_a, base_rev_b);

    let events = fx
        .store
        .search_causal_events(None, Some(BASE_ID), 50)
        .unwrap();
    let drift: Vec<_> = events
        .iter()
        .filter(|e| e.action == "revision.adapter_drift_detected")
        .collect();
    assert_eq!(drift.len(), 1, "expected exactly one drift event: {events:?}");
    let payload: serde_json::Value =
        serde_json::from_str(drift[0].payload.as_deref().unwrap_or("")).unwrap();
    assert_eq!(payload["promoted_revision"], base_rev_b);
    assert_eq!(payload["stale_wrappers"][0]["wrapper_agent_id"], WRAPPER_ID);

    // 6b. #1228: the promotion response names the staled wrappers inline —
    // the actor learns the drift without reading store surfaces.
    assert_eq!(
        base_promo_res["adapter_drift"]["stale_wrappers"][0]["wrapper_agent_id"],
        WRAPPER_ID
    );

    // 6c. #1228: the promoting root's operator feed carries exactly one
    // proactive `adapter_drift_notice`, linked to the drift causal event.
    // Rate-limited per root via the standard operator-activity window; the
    // unique causal index makes it exactly-once per promotion.
    let promo_root = format!("{SESSION}-{BASE_ID}");
    let feed = fx
        .store
        .list_operator_activity(&promo_root, None, 50, None)
        .unwrap();
    let notices: Vec<_> = feed
        .activities
        .iter()
        .filter(|a| {
            a.kind == autonoetic_types::operator_activity::OperatorActivityKind::AdapterDriftNotice
        })
        .collect();
    assert_eq!(
        notices.len(),
        1,
        "expected one adapter_drift_notice, got: {:?}",
        feed.activities
    );
    assert_eq!(
        notices[0].severity,
        autonoetic_types::operator_activity::OperatorActivitySeverity::Attention
    );
    assert_eq!(
        notices[0].causal_event_id.as_deref(),
        Some(drift[0].event_id.as_str())
    );
    assert!(
        notices[0].summary.contains(WRAPPER_ID),
        "notice should name the staled wrapper: {}",
        notices[0].summary
    );
    assert!(
        notices[0].summary.contains("nothing was regenerated"),
        "notice must stay advisory: {}",
        notices[0].summary
    );

    // 7. Roster flips: the untouched wrapper is stale.
    let agents = list_agents(&fx);
    assert_eq!(find_agent(&agents, WRAPPER_ID)["stale_base"], true);
}

struct EchoForecastDriver;

#[async_trait::async_trait]
impl LlmDriver for EchoForecastDriver {
    async fn complete(&self, req: &CompletionRequest) -> anyhow::Result<CompletionResponse> {
        // Runs after pre_map: the caller's `location` must have been renamed
        // to the base's `city`.
        let user_content = req
            .messages
            .iter()
            .rev()
            .find(|m| matches!(m.role, autonoetic_gateway::llm::Role::User))
            .map(|m| m.content.clone())
            .expect("user message should exist");
        let parsed: serde_json::Value = serde_json::from_str(&user_content)
            .expect("pre-map should produce JSON user content");
        let city = parsed
            .get("city")
            .and_then(|v| v.as_str())
            .expect("base field 'city' should be present after pre-map");
        Ok(CompletionResponse {
            text: serde_json::json!({ "summary": format!("forecast:{city}") }).to_string(),
            tool_calls: vec![],
            reasoning_content: None,
            reasoning_details: None,
            stop_reason: StopReason::EndTurn,
            usage: TokenUsage::default(),
        })
    }
}

/// Execute the generated mapping end to end: caller speaks the TARGET shape,
/// the generated middleware maps in and out, and the reply arrives in the
/// caller's shape.
#[tokio::test]
async fn real_adaptation_executes_generated_mapping() {
    if !is_bwrap_available() {
        eprintln!("skipping real_adaptation_executes_generated_mapping: `bwrap` not on PATH");
        return;
    }
    let fx = setup();
    let base_rev_a = install_agent(
        &fx,
        BASE_ID,
        "# Weather Base\nFetches weather for a city.\n",
        serde_json::json!({ "accepts": base_accepts(), "returns": base_returns() }),
    );
    let wrapper_dir = generate_wrapper_bundle(&fx, &base_rev_a);

    let skill = std::fs::read_to_string(wrapper_dir.join("SKILL.md")).unwrap();
    let (manifest, instructions) = SkillParser::parse(&skill).unwrap();
    let middleware = manifest.middleware.clone().expect("wrapper has middleware");

    // Constitution init: threading a gateway dir puts the turn on the full
    // reply path (P-6.23 attestation tail).
    if let Err(e) = autonoetic_gateway::constitution_digest::initialize_constitution(
        &autonoetic_types::config::GatewayConfig::default(),
    ) {
        assert!(
            autonoetic_gateway::constitution_digest::is_constitution_initialized(),
            "constitution runtime failed to initialize: {e}"
        );
    }

    let mut executor = AgentExecutor::new(
        manifest,
        instructions,
        Arc::new(EchoForecastDriver),
        wrapper_dir,
        default_registry(),
        None,
    )
    .with_middleware(middleware)
    .with_gateway_dir(fx.gateway_dir.clone())
    .with_session_id(SESSION);

    let mut history = vec![Message::user(r#"{"location":"paris"}"#)];
    let reply = match executor
        .execute_with_history(&mut history)
        .await
        .expect("wrapper execution should succeed")
    {
        autonoetic_gateway::runtime::lifecycle::TurnOutcome::Completed(Some(r)) => r,
        other => panic!("expected Completed reply, got {other:?}"),
    };
    let parsed: serde_json::Value = serde_json::from_str(&reply).expect("post-map emits JSON");
    assert_eq!(
        parsed,
        serde_json::json!({ "result": "forecast:paris" }),
        "caller must receive the TARGET shape"
    );
}
