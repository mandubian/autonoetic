use autonoetic_gateway::llm::{
    CompletionRequest, CompletionResponse, LlmDriver, Message, StopReason, TokenUsage,
};
use autonoetic_gateway::runtime::lifecycle::AgentExecutor;
use autonoetic_gateway::runtime::parser::SkillParser;
use autonoetic_types::capability::Capability;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;

/// Wrapper execution runs the generated pre/post map scripts inside a
/// bubblewrap sandbox. On hosts without `bwrap` (e.g. local dev boxes) those
/// tests can't run; skip them rather than fail with a spawn ENOENT — matching
/// the convention in sandbox_capture_integration.rs.
fn is_bwrap_available() -> bool {
    std::process::Command::new("bwrap")
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

struct EchoSummaryDriver;

#[async_trait::async_trait]
impl LlmDriver for EchoSummaryDriver {
    async fn complete(&self, req: &CompletionRequest) -> anyhow::Result<CompletionResponse> {
        let user_content = req
            .messages
            .iter()
            .rev()
            .find(|m| matches!(m.role, autonoetic_gateway::llm::Role::User))
            .map(|m| m.content.clone())
            .expect("user message should exist");
        let parsed: serde_json::Value =
            serde_json::from_str(&user_content).expect("pre-map should produce JSON user content");
        let query = parsed
            .get("query")
            .and_then(|v| v.as_str())
            .expect("query field should be present after pre-map");
        let text = serde_json::json!({ "summary": format!("done:{query}") }).to_string();
        Ok(CompletionResponse {
            text,
            tool_calls: vec![],
            reasoning_content: None,
            reasoning_details: None,
            stop_reason: StopReason::EndTurn,
            usage: TokenUsage::default(),
        })
    }
}

fn generate_wrapper(
    temp: &tempfile::TempDir,
    wrapper_id: &str,
    target_spec: &serde_json::Value,
    base_manifest: &serde_json::Value,
    base_accepts: &serde_json::Value,
    base_returns: &serde_json::Value,
) -> PathBuf {
    let schema_diff_script = script_path("schema_diff.py");
    let generate_wrapper_script = script_path("generate_wrapper.py");

    let diff_input = serde_json::json!({
        "base_accepts": base_accepts,
        "base_returns": base_returns,
        "target_accepts": target_spec.get("accepts"),
        "target_returns": target_spec.get("returns")
    });
    let diff = run_python_with_stdin(&schema_diff_script, &[], &diff_input);

    let base_skill_path = temp.path().join("base.SKILL.md");
    std::fs::write(
        &base_skill_path,
        r#"---
name: "base.agent"
description: "base"
metadata:
  autonoetic:
    version: "1.0"
---
# Base
Base instructions.
"#,
    )
    .expect("base skill should write");

    let output_dir = temp.path().join("wrapper");
    let out = Command::new("python3")
        .arg(&generate_wrapper_script)
        .arg("--base-skill")
        .arg(base_skill_path.to_string_lossy().to_string())
        .arg("--base-agent-id")
        .arg("base.agent")
        .arg("--wrapper-id")
        .arg(wrapper_id)
        .arg("--target-spec-json")
        .arg(serde_json::to_string(target_spec).expect("target spec serializes"))
        .arg("--schema-diff-json")
        .arg(serde_json::to_string(&diff).expect("diff serializes"))
        .arg("--base-manifest-json")
        .arg(serde_json::to_string(base_manifest).expect("base manifest serializes"))
        .arg("--output-dir")
        .arg(output_dir.to_string_lossy().to_string())
        .output()
        .expect("generator should execute");

    assert!(
        out.status.success(),
        "generate_wrapper.py failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    output_dir
}

#[tokio::test]
async fn test_generated_wrapper_executes_with_io_transformation() {
    if !is_bwrap_available() {
        eprintln!("skipping test_generated_wrapper_executes_with_io_transformation: `bwrap` not on PATH");
        return;
    }
    let temp = tempfile::tempdir().expect("tempdir should create");
    let gateway_dir = temp.path().join("runtime");
    std::fs::create_dir_all(&gateway_dir).expect("gateway dir should create");
    // Threading a gateway dir puts the turn on the full reply path, which reads
    // the constitution version for the state-attestation tail (P-6.23). A
    // config-mismatch error only means a neighbor test initialized first.
    if let Err(e) = autonoetic_gateway::constitution_digest::initialize_constitution(
        &autonoetic_types::config::GatewayConfig::default(),
    ) {
        assert!(
            autonoetic_gateway::constitution_digest::is_constitution_initialized(),
            "constitution runtime failed to initialize: {e}"
        );
    }
    let target_spec = serde_json::json!({
        "accepts": {
            "type": "object",
            "required": ["task"],
            "properties": { "task": { "type": "string" } }
        },
        "returns": {
            "type": "object",
            "required": ["result"],
            "properties": { "result": { "type": "string" } }
        }
    });
    let base_manifest = serde_json::json!({ "capabilities": [] });
    let wrapper_dir = generate_wrapper(
        &temp,
        "base.agent.adapter",
        &target_spec,
        &base_manifest,
        &serde_json::json!({
            "type": "object",
            "required": ["query"],
            "properties": { "query": { "type": "string" } }
        }),
        &serde_json::json!({
            "type": "object",
            "required": ["summary"],
            "properties": { "summary": { "type": "string" } }
        }),
    );

    let skill_content =
        std::fs::read_to_string(wrapper_dir.join("SKILL.md")).expect("wrapper skill should read");
    let (manifest, instructions) =
        SkillParser::parse(&skill_content).expect("wrapper should parse");
    let middleware = manifest
        .middleware
        .clone()
        .expect("wrapper should declare middleware");

    let mut executor = AgentExecutor::new(
        manifest,
        instructions,
        Arc::new(EchoSummaryDriver),
        wrapper_dir,
        autonoetic_gateway::runtime::tools::default_registry(),
        None,
    )
    .with_middleware(middleware)
    // Middleware runs in a sandbox, and the sandbox's gateway-secret mask is
    // built from this path — running without it now fails closed rather than
    // spawning unmasked (#1145).
    .with_gateway_dir(gateway_dir.clone())
    .with_session_id("session-wrapper-io");

    let mut history = vec![Message::user(r#"{"task":"demo"}"#)];
    let reply = match executor
        .execute_with_history(&mut history)
        .await
        .expect("wrapper execution should succeed")
    {
        autonoetic_gateway::runtime::lifecycle::TurnOutcome::Completed(Some(r)) => r,
        other => panic!("expected Completed reply, got {:?}", other),
    };

    let parsed_reply: serde_json::Value =
        serde_json::from_str(&reply).expect("post-map should emit JSON");
    assert_eq!(
        parsed_reply.get("result"),
        Some(&serde_json::json!("done:demo"))
    );
}

#[test]
fn test_generated_wrapper_inherits_base_capabilities() {
    let temp = tempfile::tempdir().expect("tempdir should create");
    let target_spec = serde_json::json!({
        "accepts": { "type": "object", "required": ["query"] },
        "returns": { "type": "object", "required": ["summary"] }
    });
    let base_manifest = serde_json::json!({
        "capabilities": [
            { "type": "SandboxFunctions", "allowed": ["web_search"] },
            { "type": "ReadAccess", "scopes": ["*"] }
        ]
    });
    let wrapper_dir = generate_wrapper(
        &temp,
        "base.agent.adapter.cap",
        &target_spec,
        &base_manifest,
        &serde_json::json!({
            "type": "object",
            "required": ["query"],
            "properties": { "query": { "type": "string" } }
        }),
        &serde_json::json!({
            "type": "object",
            "required": ["summary"],
            "properties": { "summary": { "type": "string" } }
        }),
    );
    let skill_content =
        std::fs::read_to_string(wrapper_dir.join("SKILL.md")).expect("wrapper skill should read");
    let (manifest, _instructions) =
        SkillParser::parse(&skill_content).expect("wrapper should parse");

    assert_eq!(manifest.capabilities.len(), 2);
    assert!(matches!(
        &manifest.capabilities[0],
        Capability::SandboxFunctions { allowed } if allowed == &vec!["web_search".to_string()]
    ));
    assert!(matches!(
        &manifest.capabilities[1],
        Capability::ReadAccess { scopes } if scopes == &vec!["*".to_string()]
    ));
}

/// The inherited inference block must still parse as a manifest — a wrapper
/// that carries the base's preset is only useful if the gateway can load it.
#[test]
fn test_generated_wrapper_manifest_carries_inherited_preset() {
    let temp = tempfile::tempdir().expect("tempdir should create");
    let target_spec = serde_json::json!({
        "accepts": { "type": "object", "required": ["query"] },
        "returns": { "type": "object", "required": ["summary"] }
    });
    let base_manifest = serde_json::json!({
        "capabilities": [],
        "llm_preset": "coding"
    });
    let wrapper_dir = generate_wrapper(
        &temp,
        "base.agent.adapter.preset",
        &target_spec,
        &base_manifest,
        &serde_json::json!({ "type": "object", "required": ["query"] }),
        &serde_json::json!({ "type": "object", "required": ["summary"] }),
    );
    let skill_content =
        std::fs::read_to_string(wrapper_dir.join("SKILL.md")).expect("wrapper skill should read");
    let (manifest, _instructions) =
        SkillParser::parse(&skill_content).expect("wrapper with inherited preset should parse");

    assert_eq!(manifest.llm_preset.as_deref(), Some("coding"));
    assert!(
        manifest.llm_config.is_none(),
        "preset-based wrappers must leave provider/model to gateway config"
    );
}

/// Composition provenance (#1204): the `adapter:` block the generator emits
/// must survive the parser as a first-class `AgentManifest.adapter` — it used
/// to be silently dropped, so lineage existed only as raw SKILL.md bytes.
#[test]
fn test_generated_wrapper_provenance_parses_with_base_digest() {
    let temp = tempfile::tempdir().expect("tempdir should create");
    let target_spec = serde_json::json!({
        "accepts": { "type": "object", "required": ["query"] },
        "returns": { "type": "object", "required": ["summary"] }
    });
    let base_manifest = serde_json::json!({ "capabilities": [] });
    let wrapper_dir = generate_wrapper(
        &temp,
        "base.agent.adapter.prov",
        &target_spec,
        &base_manifest,
        &serde_json::json!({ "type": "object", "required": ["query"] }),
        &serde_json::json!({ "type": "object", "required": ["summary"] }),
    );
    let skill_content =
        std::fs::read_to_string(wrapper_dir.join("SKILL.md")).expect("wrapper skill should read");
    let (manifest, _instructions) =
        SkillParser::parse(&skill_content).expect("wrapper should parse");

    let adapter = manifest
        .adapter
        .expect("adapter provenance should parse — the block must not be dropped");
    assert_eq!(adapter.base_agent_id, "base.agent");
    assert_eq!(adapter.generator.as_deref(), Some("agent-adapter.default"));
    assert!(
        adapter.base_revision_digest.is_none(),
        "generator was not given a digest — provenance must under-claim, not guess"
    );
    assert!(!adapter.schema_notes.is_empty());
    assert!(adapter.generated_at.is_some());
}

/// Same, but with `--base-revision-digest` supplied: the digest must round-trip
/// into the parsed manifest so Phase 2 drift detection has something to compare.
#[test]
fn test_generated_wrapper_provenance_records_base_revision_digest() {
    let temp = tempfile::tempdir().expect("tempdir should create");
    let base_skill_path = temp.path().join("base.SKILL.md");
    std::fs::write(
        &base_skill_path,
        "---\nname: \"base.agent\"\ndescription: \"base\"\n---\n# Base\n",
    )
    .expect("base skill should write");
    let output_dir = temp.path().join("wrapper");
    let out = Command::new("python3")
        .arg(script_path("generate_wrapper.py"))
        .arg("--base-skill")
        .arg(base_skill_path.to_string_lossy().to_string())
        .arg("--base-agent-id")
        .arg("base.agent")
        .arg("--wrapper-id")
        .arg("base.agent.adapter.digest")
        .arg("--target-spec-json")
        .arg(r#"{"accepts":{"type":"object","required":["query"]},"returns":{"type":"object","required":["summary"]}}"#)
        .arg("--schema-diff-json")
        .arg(r#"{"accepts_compatible":true,"returns_compatible":true,"requires_input_mapping":false,"requires_output_mapping":false,"input_mappings":[],"output_mappings":[],"notes":[]}"#)
        .arg("--base-revision-digest")
        .arg("rev_sha256:deadbeef")
        .arg("--output-dir")
        .arg(output_dir.to_string_lossy().to_string())
        .output()
        .expect("generator should execute");
    assert!(
        out.status.success(),
        "generate_wrapper.py failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let skill_content =
        std::fs::read_to_string(output_dir.join("SKILL.md")).expect("wrapper skill should read");
    let (manifest, _instructions) =
        SkillParser::parse(&skill_content).expect("wrapper should parse");
    let adapter = manifest
        .adapter
        .expect("adapter provenance should parse — the block must not be dropped");
    assert_eq!(
        adapter.base_revision_digest.as_deref(),
        Some("rev_sha256:deadbeef")
    );
}

struct EchoSummaryConfidenceDriver;

#[async_trait::async_trait]
impl LlmDriver for EchoSummaryConfidenceDriver {
    async fn complete(&self, req: &CompletionRequest) -> anyhow::Result<CompletionResponse> {
        let user_content = req
            .messages
            .iter()
            .rev()
            .find(|m| matches!(m.role, autonoetic_gateway::llm::Role::User))
            .map(|m| m.content.clone())
            .expect("user message should exist");
        let parsed: serde_json::Value =
            serde_json::from_str(&user_content).expect("pre-map should produce JSON user content");
        let query = parsed
            .get("query")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        let domain = parsed
            .get("domain")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        let text = serde_json::json!({
            "summary": format!("done:{query}:{domain}"),
            "confidence": "high"
        })
        .to_string();
        Ok(CompletionResponse {
            text,
            tool_calls: vec![],
            reasoning_content: None,
            reasoning_details: None,
            stop_reason: StopReason::EndTurn,
            usage: TokenUsage::default(),
        })
    }
}

#[tokio::test]
async fn test_generated_wrapper_executes_with_multiple_io_transformations() {
    if !is_bwrap_available() {
        eprintln!("skipping test_generated_wrapper_executes_with_multiple_io_transformations: `bwrap` not on PATH");
        return;
    }
    let temp = tempfile::tempdir().expect("tempdir should create");
    let gateway_dir = temp.path().join("runtime");
    std::fs::create_dir_all(&gateway_dir).expect("gateway dir should create");
    // Threading a gateway dir puts the turn on the full reply path, which reads
    // the constitution version for the state-attestation tail (P-6.23). A
    // config-mismatch error only means a neighbor test initialized first.
    if let Err(e) = autonoetic_gateway::constitution_digest::initialize_constitution(
        &autonoetic_types::config::GatewayConfig::default(),
    ) {
        assert!(
            autonoetic_gateway::constitution_digest::is_constitution_initialized(),
            "constitution runtime failed to initialize: {e}"
        );
    }
    let target_spec = serde_json::json!({
        "accepts": {
            "type": "object",
            "required": ["task", "topic"]
        },
        "returns": {
            "type": "object",
            "required": ["result", "score"]
        }
    });
    let base_manifest = serde_json::json!({ "capabilities": [] });
    let wrapper_dir = generate_wrapper(
        &temp,
        "base.agent.adapter.multi",
        &target_spec,
        &base_manifest,
        &serde_json::json!({
            "type": "object",
            "required": ["query", "domain"]
        }),
        &serde_json::json!({
            "type": "object",
            "required": ["summary", "confidence"]
        }),
    );

    let skill_content =
        std::fs::read_to_string(wrapper_dir.join("SKILL.md")).expect("wrapper skill should read");
    let (manifest, instructions) =
        SkillParser::parse(&skill_content).expect("wrapper should parse");
    let middleware = manifest
        .middleware
        .clone()
        .expect("wrapper should declare middleware");

    let mut executor = AgentExecutor::new(
        manifest,
        instructions,
        Arc::new(EchoSummaryConfidenceDriver),
        wrapper_dir,
        autonoetic_gateway::runtime::tools::default_registry(),
        None,
    )
    .with_middleware(middleware)
    // Middleware runs in a sandbox, and the sandbox's gateway-secret mask is
    // built from this path — running without it now fails closed rather than
    // spawning unmasked (#1145).
    .with_gateway_dir(gateway_dir.clone())
    .with_session_id("session-wrapper-io-multi");

    let mut history = vec![Message::user(r#"{"task":"demo","topic":"ops"}"#)];
    let reply = match executor
        .execute_with_history(&mut history)
        .await
        .expect("wrapper execution should succeed")
    {
        autonoetic_gateway::runtime::lifecycle::TurnOutcome::Completed(Some(r)) => r,
        other => panic!("expected Completed reply, got {:?}", other),
    };

    let parsed_reply: serde_json::Value =
        serde_json::from_str(&reply).expect("post-map should emit JSON");
    assert_eq!(
        parsed_reply.get("result"),
        Some(&serde_json::json!("done:demo:ops"))
    );
    assert_eq!(parsed_reply.get("score"), Some(&serde_json::json!("high")));
}
