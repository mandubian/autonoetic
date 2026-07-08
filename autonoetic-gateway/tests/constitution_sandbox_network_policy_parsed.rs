//! Constitution test for RFC scope 5.1 — `SandboxNetworkPolicy` manifest
//! field parsing and the refuse-boot guard for `Recording` mode.
//!
//! Pins:
//!
//! 1. Omitting `sandbox_network` from a SKILL.md frontmatter parses
//!    successfully and defaults to `Normal`.
//! 2. Explicit `sandbox_network: normal | sealed | recording` round-trip
//!    through the parser into the typed variants.
//! 3. Unknown / mis-spelled values are rejected at manifest load with
//!    a clear error.
//! 4. A session whose manifest declares `sandbox_network: recording`
//!    refuses to start when the gateway config does not have
//!    `sandbox.allow_recording: true`.
//! 5. The same session starts cleanly when the operator has opted in
//!    via `sandbox.allow_recording: true`. (Note: until scopes 5.2/5.3
//!    ship, `Sealed` and `Recording` are otherwise dormant — declaring
//!    them produces no runtime effect beyond this guard.)
//!
//! Refs: docs/archived/sealed-network-evaluation-plan.md §3.1 / scope 5.1.

mod support;

use autonoetic_gateway::runtime::lifecycle::{AgentExecutor, TurnOutcome};
use autonoetic_gateway::runtime::parser::SkillParser;
use autonoetic_gateway::runtime::tools::default_registry;
use autonoetic_types::agent::SandboxNetworkPolicy;
use autonoetic_types::config::GatewayConfig;
use std::sync::Arc;
use tempfile::tempdir;

fn make_skill(agent_id: &str, sandbox_network_line: Option<&str>) -> String {
    let network_line = sandbox_network_line
        .map(|v| format!("    sandbox_network: {v}\n"))
        .unwrap_or_default();
    format!(
        r#"---
name: "{agent_id}"
description: "Test agent for sandbox-network policy parsing"
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
      id: "{agent_id}"
      name: "{agent_id}"
      description: "Test agent"
    capabilities: []
    llm_config:
      provider: "openai"
      model: "test-model"
{network_line}---
# {agent_id}
"#,
        agent_id = agent_id,
        network_line = network_line,
    )
}

#[test]
fn manifest_without_sandbox_network_defaults_to_normal() {
    let skill = make_skill("default.network", None);
    let (manifest, _instructions) = SkillParser::parse(&skill).expect("parse must succeed");
    assert_eq!(
        manifest.sandbox_network,
        SandboxNetworkPolicy::Normal,
        "omitted sandbox_network must default to Normal"
    );
}

#[test]
fn manifest_with_sandbox_network_normal_parses() {
    let skill = make_skill("explicit.normal", Some("normal"));
    let (manifest, _) = SkillParser::parse(&skill).expect("parse must succeed");
    assert_eq!(manifest.sandbox_network, SandboxNetworkPolicy::Normal);
}

#[test]
fn manifest_with_sandbox_network_sealed_parses() {
    let skill = make_skill("explicit.sealed", Some("sealed"));
    let (manifest, _) = SkillParser::parse(&skill).expect("parse must succeed");
    assert_eq!(manifest.sandbox_network, SandboxNetworkPolicy::Sealed);
}

#[test]
fn manifest_with_sandbox_network_recording_parses() {
    let skill = make_skill("explicit.recording", Some("recording"));
    let (manifest, _) = SkillParser::parse(&skill).expect("parse must succeed");
    assert_eq!(manifest.sandbox_network, SandboxNetworkPolicy::Recording);
}

#[test]
fn manifest_with_unknown_sandbox_network_value_rejected() {
    let skill = make_skill("bad.value", Some("yolo"));
    let err = SkillParser::parse(&skill).expect_err(
        "unknown sandbox_network value must reject at manifest load",
    );
    let msg = err.to_string().to_lowercase();
    assert!(
        msg.contains("sandbox_network") || msg.contains("yolo") || msg.contains("variant"),
        "rejection should mention the invalid field/value; got: {}",
        err
    );
}

#[test]
fn manifest_with_misspelled_sandbox_network_value_rejected() {
    // Common misspellings the parser must reject rather than silently
    // accept as the default.
    for bad in ["seal", "Sealed", "record", "RECORDING", "fixture", "off"] {
        let skill = make_skill("bad.spelling", Some(bad));
        let err = SkillParser::parse(&skill).expect_err(&format!(
            "value '{}' must be rejected by the parser",
            bad
        ));
        assert!(
            err.to_string().to_lowercase().contains(&bad.to_lowercase())
                || err.to_string().contains("variant"),
            "rejection should reference the bad value '{}'; got: {}",
            bad,
            err
        );
    }
}

// ── Refuse-boot guard ────────────────────────────────────────────

/// Stub LLM driver that should never be called in the refuse-boot tests
/// — the guard fires before any LLM turn runs.
struct UnreachableDriver;

#[async_trait::async_trait]
impl autonoetic_gateway::llm::LlmDriver for UnreachableDriver {
    async fn complete(
        &self,
        _req: &autonoetic_gateway::llm::CompletionRequest,
    ) -> anyhow::Result<autonoetic_gateway::llm::CompletionResponse> {
        panic!("LLM should never be called when sandbox_network refuse-boot fires")
    }
}

fn build_executor(skill: &str, config: GatewayConfig) -> AgentExecutor {
    let (manifest, instructions) = SkillParser::parse(skill).expect("parse");
    let tmp = tempdir().expect("tempdir");
    let agent_dir = tmp.path().to_path_buf();
    let _keep = Box::leak(Box::new(tmp));
    AgentExecutor::new(
        manifest,
        instructions,
        Arc::new(UnreachableDriver),
        agent_dir,
        default_registry(),
        None,
    )
    .with_config(Arc::new(config))
    .with_session_id("refuse-boot-session".to_string())
}

#[tokio::test]
async fn recording_session_refuses_to_start_without_operator_optin() {
    let skill = make_skill("recording.no.optin", Some("recording"));
    let mut config = GatewayConfig::default();
    config.sandbox.allow_recording = false; // explicit, even though it is the default

    let mut executor = build_executor(&skill, config);
    let mut history = Vec::new();
    let result = executor.execute_with_history(&mut history).await;
    let err = result.expect_err(
        "manifest sandbox_network=recording must refuse to start without gateway optin",
    );
    let msg = err.to_string();
    assert!(
        msg.contains("Session refused to start") && msg.contains("recording"),
        "refuse-boot error should mention session refusal and recording mode; got: {}",
        msg
    );
    assert!(
        msg.contains("allow_recording"),
        "refuse-boot error should name the config flag operators must flip; got: {}",
        msg
    );
}

#[tokio::test]
async fn recording_session_starts_when_operator_has_opted_in() {
    // With the config flag set, the refuse-boot guard does not fire. The
    // session reaches the LLM driver (which then panics in our stub),
    // confirming the guard does not block. We catch the driver panic via
    // catch_unwind to keep the test deterministic.
    let skill = make_skill("recording.with.optin", Some("recording"));
    let mut config = GatewayConfig::default();
    config.sandbox.allow_recording = true;

    let mut executor = build_executor(&skill, config);
    let mut history = Vec::new();
    let outcome = std::panic::AssertUnwindSafe(executor.execute_with_history(&mut history));
    let result = futures::FutureExt::catch_unwind(outcome).await;

    // We expect either an Err from the LLM call (unwrapping the panic),
    // or a panic — both prove the guard did NOT fire. What we MUST NOT
    // see is the specific refuse-boot error string.
    match result {
        Ok(Ok(TurnOutcome::Completed(_)))
        | Ok(Ok(TurnOutcome::Suspended { .. }))
        | Ok(Ok(TurnOutcome::SuspendedUserInput { .. }))
        | Ok(Ok(TurnOutcome::WaitingForChild { .. }))
        | Ok(Ok(TurnOutcome::Escalated { .. })) => {
            panic!("Stub LLM should not return an outcome — guard interaction is suspicious")
        }
        Ok(Err(e)) => {
            let msg = e.to_string();
            assert!(
                !msg.contains("Session refused to start"),
                "guard fired even with allow_recording=true: {}",
                msg
            );
        }
        Err(panic_payload) => {
            // The stub driver panicked, meaning the guard let us through.
            // Confirm the panic message is the driver's, not the guard's.
            let msg = panic_payload
                .downcast::<String>()
                .map(|s| *s)
                .or_else(|p| p.downcast::<&'static str>().map(|s| s.to_string()))
                .unwrap_or_else(|_| "(unknown panic)".to_string());
            assert!(
                msg.contains("LLM should never be called")
                    || !msg.contains("Session refused to start"),
                "guard fired (panicked from refuse-boot) even with optin: {}",
                msg
            );
        }
    }
}

#[tokio::test]
async fn sealed_session_starts_without_operator_optin() {
    // The optin flag gates only Recording, not Sealed. A manifest with
    // sandbox_network=sealed must start without any operator flag — the
    // sealed-egress hook itself (scope 5.2) is what enforces seal behaviour.
    let skill = make_skill("sealed.no.optin", Some("sealed"));
    let mut config = GatewayConfig::default();
    config.sandbox.allow_recording = false;

    let mut executor = build_executor(&skill, config);
    let mut history = Vec::new();
    let outcome = std::panic::AssertUnwindSafe(executor.execute_with_history(&mut history));
    let result = futures::FutureExt::catch_unwind(outcome).await;

    match result {
        Ok(Err(e)) => {
            assert!(
                !e.to_string().contains("Session refused to start"),
                "sealed mode must not trigger the refuse-boot guard: {}",
                e
            );
        }
        Err(panic_payload) => {
            // Stub driver panicked — guard let us through. OK.
            let msg = panic_payload
                .downcast::<String>()
                .map(|s| *s)
                .or_else(|p| p.downcast::<&'static str>().map(|s| s.to_string()))
                .unwrap_or_else(|_| "(unknown panic)".to_string());
            assert!(
                !msg.contains("Session refused to start"),
                "sealed mode triggered refuse-boot (it should not): {}",
                msg
            );
        }
        Ok(Ok(_)) => panic!("stub driver should have failed"),
    }
}
