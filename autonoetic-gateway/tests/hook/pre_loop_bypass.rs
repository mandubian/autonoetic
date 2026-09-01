//! Pre-loop bypass for bypass-declared middleware hooks (#1223).
//!
//! `middleware.bypass: true` declares the pre-hook a pure function of the
//! conversation payload: it either answers (`skip_llm` + `assistant_reply`)
//! or declines. The hook is evaluated exactly once per turn, BEFORE the
//! reasoning loop assembles tool schemas and composes the system prompt:
//!
//! - a `skip` ends the turn without building any completion — the LLM driver
//!   must never be called;
//! - a `proceed` hands the request to the loop unmodified, and the hook is
//!   NOT run a second time;
//! - hooks without the declaration keep the legacy in-loop contract, where a
//!   `skip_llm` short-circuits only the completion itself.
//!
//! Hooks run in a bubblewrap sandbox; skipped when bwrap is unavailable.

use autonoetic_gateway::llm::{CompletionRequest, CompletionResponse, LlmDriver, StopReason, TokenUsage};
use autonoetic_gateway::runtime::lifecycle::AgentExecutor;
use autonoetic_gateway::runtime::parser::SkillParser;
use autonoetic_gateway::runtime::tools::default_registry;
use autonoetic_types::agent::Middleware;
use std::path::{Path, PathBuf};
use std::sync::Arc;

fn is_bwrap_available() -> bool {
    std::process::Command::new("bwrap")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn write_agent(dir: &Path, middleware_yaml: &str, hook_script: &str) {
    std::fs::create_dir_all(dir.join("scripts")).expect("scripts dir");
    std::fs::write(dir.join("scripts/hook.py"), hook_script).expect("hook write");
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
  id: "bypass.test"
  name: "bypass.test"
  description: "Bypass middleware test agent"
llm_config:
  provider: "openai"
  model: "test-model"
middleware:
{middleware_yaml}
---
# Bypass Test Agent
"#,
        middleware_yaml = middleware_yaml
    );
    std::fs::write(dir.join("SKILL.md"), skill).expect("skill write");
}

/// A driver whose completion attempt IS the assertion: if the gateway ever
/// calls it on a turn that must be bypassed, the turn fails loudly.
struct NeverCalledDriver;

#[async_trait::async_trait]
impl LlmDriver for NeverCalledDriver {
    async fn complete(&self, _req: &CompletionRequest) -> anyhow::Result<CompletionResponse> {
        anyhow::bail!("LLM must not be called on a bypassed turn")
    }
}

struct FixedReplyDriver {
    reply: &'static str,
}

#[async_trait::async_trait]
impl LlmDriver for FixedReplyDriver {
    async fn complete(&self, _req: &CompletionRequest) -> anyhow::Result<CompletionResponse> {
        Ok(CompletionResponse {
            text: self.reply.to_string(),
            tool_calls: vec![],
            reasoning_content: None,
            reasoning_details: None,
            stop_reason: StopReason::EndTurn,
            usage: TokenUsage::default(),
        })
    }
}

async fn run_turn(
    agent_dir: PathBuf,
    driver: Arc<dyn LlmDriver>,
) -> autonoetic_gateway::runtime::lifecycle::TurnOutcome {
    // The loop path composes the system prompt, whose tail reads the
    // constitution version (P-6.23). A config-mismatch error only means a
    // neighbor test initialized first. Bypassed turns skip composition
    // entirely — that is the point (#1223) — so only the loop-entry tests
    // exercise this.
    if let Err(e) = autonoetic_gateway::constitution_digest::initialize_constitution(
        &autonoetic_types::config::GatewayConfig::default(),
    ) {
        assert!(
            autonoetic_gateway::constitution_digest::is_constitution_initialized(),
            "constitution runtime failed to initialize: {e}"
        );
    }

    let skill = std::fs::read_to_string(agent_dir.join("SKILL.md")).expect("skill read");
    let (manifest, instructions) = SkillParser::parse(&skill).expect("manifest parses");
    let middleware = manifest.middleware.clone().expect("middleware declared");

    let mut executor = AgentExecutor::new(
        manifest,
        instructions,
        driver,
        agent_dir.clone(),
        default_registry(),
        None,
    )
    .with_middleware(middleware)
    .with_gateway_dir(agent_dir.join(".gateway"))
    .with_session_id("session-bypass-test");

    let mut history = vec![autonoetic_gateway::llm::Message::user(r#"{"q":"hi"}"#)];
    match executor.execute_with_history(&mut history).await {
        Ok(outcome) => outcome,
        other => panic!("turn should complete, got {:?}", other.err()),
    }
}

/// A bypass-declared hook that answers ends the turn deterministically: the
/// reply is the hook's, and no completion is ever built.
#[tokio::test]
async fn bypass_hook_answers_without_building_a_completion() {
    if !is_bwrap_available() {
        eprintln!("skipping: bwrap not available");
        return;
    }
    let temp = tempfile::tempdir().expect("tempdir");
    let agent_dir = temp.path().join("bypass.test");
    write_agent(
        &agent_dir,
        "  pre_process: \"python3 scripts/hook.py\"\n  bypass: true\n",
        r#"import json, sys
req = json.load(sys.stdin)
# The probe envelope carries the conversation only — assert that contract.
assert "tools" in req, "probe must be a CompletionRequest-shaped envelope"
print(json.dumps({"skip_llm": True, "assistant_reply": "deterministic bypass reply"}))
"#,
    );

    match run_turn(agent_dir, Arc::new(NeverCalledDriver)).await {
        autonoetic_gateway::runtime::lifecycle::TurnOutcome::Completed(Some(reply)) => {
            assert_eq!(reply, "deterministic bypass reply");
        }
        other => panic!("expected Completed reply, got {:?}", other),
    }
}

/// A bypass-declared hook that declines hands the request to the loop
/// unmodified — and is NOT evaluated a second time inside the loop. The
/// hook's own counter file pins the exactly-once contract.
#[tokio::test]
async fn bypass_hook_decline_runs_the_loop_with_single_hook_evaluation() {
    if !is_bwrap_available() {
        eprintln!("skipping: bwrap not available");
        return;
    }
    let temp = tempfile::tempdir().expect("tempdir");
    let agent_dir = temp.path().join("bypass.test");
    write_agent(
        &agent_dir,
        "  pre_process: \"python3 scripts/hook.py\"\n  bypass: true\n",
        r#"import json, sys
count = 0
try:
    with open("hook_evaluations") as f:
        count = int(f.read().strip() or "0")
except FileNotFoundError:
    pass
count += 1
with open("hook_evaluations", "w") as f:
    f.write(str(count))
print(json.dumps({"skip_llm": False}))
"#,
    );

    match run_turn(agent_dir.clone(), Arc::new(FixedReplyDriver { reply: "loop reply" })).await {
        autonoetic_gateway::runtime::lifecycle::TurnOutcome::Completed(Some(reply)) => {
            assert_eq!(reply, "loop reply");
        }
        other => panic!("expected Completed reply, got {:?}", other),
    }

    let evaluations = std::fs::read_to_string(agent_dir.join("hook_evaluations"))
        .expect("hook counter file");
    assert_eq!(
        evaluations.trim(),
        "1",
        "a bypass-declared hook must be evaluated exactly once per turn"
    );
}

/// Legacy parity: without the `bypass` declaration, a hook that returns
/// `skip_llm` keeps the in-loop contract — single in-loop evaluation, the
/// completion short-circuits, and the LLM is never called.
#[tokio::test]
async fn undeclared_skip_hook_keeps_the_legacy_in_loop_shortcircuit() {
    if !is_bwrap_available() {
        eprintln!("skipping: bwrap not available");
        return;
    }
    let temp = tempfile::tempdir().expect("tempdir");
    let agent_dir = temp.path().join("bypass.test");
    write_agent(
        &agent_dir,
        "  pre_process: \"python3 scripts/hook.py\"\n",
        r#"import json, sys
count = 0
try:
    with open("hook_evaluations") as f:
        count = int(f.read().strip() or "0")
except FileNotFoundError:
    pass
count += 1
with open("hook_evaluations", "w") as f:
    f.write(str(count))
print(json.dumps({"skip_llm": True, "assistant_reply": "legacy deterministic reply"}))
"#,
    );

    match run_turn(agent_dir.clone(), Arc::new(NeverCalledDriver)).await {
        autonoetic_gateway::runtime::lifecycle::TurnOutcome::Completed(Some(reply)) => {
            assert_eq!(reply, "legacy deterministic reply");
        }
        other => panic!("expected Completed reply, got {:?}", other),
    }

    let evaluations = std::fs::read_to_string(agent_dir.join("hook_evaluations"))
        .expect("hook counter file");
    assert_eq!(
        evaluations.trim(),
        "1",
        "the legacy in-loop hook must run exactly once"
    );
}

/// A bypass-declared hook that fails fails the turn (same fail-closed
/// semantics as an in-loop hook failure).
#[tokio::test]
async fn failing_bypass_hook_fails_the_turn() {
    if !is_bwrap_available() {
        eprintln!("skipping: bwrap not available");
        return;
    }
    let temp = tempfile::tempdir().expect("tempdir");
    let agent_dir = temp.path().join("bypass.test");
    write_agent(
        &agent_dir,
        "  pre_process: \"python3 scripts/hook.py\"\n  bypass: true\n",
        r#"import sys
print("hook exploded", file=sys.stderr)
sys.exit(3)
"#,
    );

    let skill = std::fs::read_to_string(agent_dir.join("SKILL.md")).expect("skill read");
    let (manifest, instructions) = SkillParser::parse(&skill).expect("manifest parses");
    let middleware = manifest.middleware.clone().expect("middleware declared");
    let mut executor = AgentExecutor::new(
        manifest,
        instructions,
        Arc::new(NeverCalledDriver),
        agent_dir.clone(),
        default_registry(),
        None,
    )
    .with_middleware(middleware)
    .with_gateway_dir(agent_dir.join(".gateway"))
    .with_session_id("session-bypass-fail");

    let mut history = vec![autonoetic_gateway::llm::Message::user(r#"{"q":"hi"}"#)];
    let result = executor.execute_with_history(&mut history).await;
    let err = result.expect_err("a failing bypass hook must fail the turn");
    assert!(
        err.to_string().to_lowercase().contains("hook"),
        "the error should name the hook failure, got: {err}"
    );
}

/// The `bypass` flag must survive the manifest parser.
#[test]
fn parser_reads_middleware_bypass_flag() {
    let content = r#"---
name: "bypass.test"
description: "test"
metadata:
  autonoetic:
    version: "1.0"
    runtime:
      engine: "autonoetic"
      gateway_version: "0.1.0"
      sdk_version: "0.1.0"
      type: "stateful"
      sandbox: "bubblewrap"
      runtime_lock: "runtime.lock"
    agent:
      id: "bypass.test"
      name: "bypass.test"
      description: "test"
    middleware:
      pre_process: "python3 scripts/hook.py"
      bypass: true
---
# body
"#;
    let (manifest, _) = SkillParser::parse(content).expect("should parse");
    let middleware: Middleware = manifest.middleware.expect("middleware should parse");
    assert_eq!(middleware.bypass, Some(true));
}
