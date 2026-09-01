//! Script-mode middleware (#1222): `pre_process` transforms the normalized
//! task payload before the entrypoint runs, `post_process` transforms stdout
//! before it becomes the reply. The contract is verbatim stdin→stdout, so
//! hooks written to that contract — the shape a script-mode adapter wrapper's
//! mapping needs — run unchanged. (The adapter generator's current LLM-envelope
//! scripts are a different contract; making generation script-mode-aware is a
//! follow-up.) Hooks run in the same sandbox profile as the entrypoint and
//! fail closed.

use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_gateway::GatewayExecutionService;
use crate::support::{seed_agent_revision, TestWorkspace};
use std::process::Command;
use std::sync::Arc;

/// The adapter shape: caller speaks `{location}`, the base speaks `{city}` /
/// `{summary}`, and the hooks map in and out — the same rename the generated
/// wrapper middleware performs in the LLM path.
fn install_mapping_agent(agent_dir: &std::path::Path, agent_id: &str) -> anyhow::Result<()> {
    std::fs::create_dir_all(agent_dir.join("scripts"))?;

    std::fs::write(
        agent_dir.join("scripts/pre_map.py"),
        r#"#!/usr/bin/env python3
import json, sys
payload = json.load(sys.stdin)
print(json.dumps({"city": payload["location"]}))
"#,
    )?;
    std::fs::write(
        agent_dir.join("scripts/entry.py"),
        r#"#!/usr/bin/env python3
import json, sys
payload = json.load(sys.stdin)
print(json.dumps({"summary": f"forecast:{payload['city']}"}))
"#,
    )?;
    std::fs::write(
        agent_dir.join("scripts/post_map.py"),
        r#"#!/usr/bin/env python3
import json, sys
payload = json.load(sys.stdin)
print(json.dumps({"result": payload["summary"]}))
"#,
    )?;

    let skill_md = format!(
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
  id: "{agent_id}"
  name: "{agent_id}"
  description: "Script agent with boundary middleware"
execution_mode: script
script_entry: scripts/entry.py
middleware:
  pre_process: "python3 scripts/pre_map.py"
  post_process: "python3 scripts/post_map.py"
capabilities: []
---
# Mapping Script Agent
"#,
    );
    std::fs::write(agent_dir.join("SKILL.md"), skill_md)?;
    std::fs::write(agent_dir.join("runtime.lock"), "dependencies: []")?;
    Ok(())
}

fn install_failing_hook_agent(agent_dir: &std::path::Path, agent_id: &str) -> anyhow::Result<()> {
    std::fs::create_dir_all(agent_dir.join("scripts"))?;

    std::fs::write(
        agent_dir.join("scripts/entry.py"),
        r#"#!/usr/bin/env python3
import json, sys
print(json.dumps({"ok": True}))
"#,
    )?;
    std::fs::write(
        agent_dir.join("scripts/broken_hook.py"),
        r#"#!/usr/bin/env python3
import sys
print("hook exploded", file=sys.stderr)
sys.exit(3)
"#,
    )?;

    let skill_md = format!(
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
  id: "{agent_id}"
  name: "{agent_id}"
  description: "Script agent whose pre hook fails"
execution_mode: script
script_entry: scripts/entry.py
middleware:
  pre_process: "python3 scripts/broken_hook.py"
capabilities: []
---
# Failing Hook Script Agent
"#,
    );
    std::fs::write(agent_dir.join("SKILL.md"), skill_md)?;
    std::fs::write(agent_dir.join("runtime.lock"), "dependencies: []")?;
    Ok(())
}

fn setup_store(
    config: &autonoetic_types::config::GatewayConfig,
    agents_dir: &std::path::Path,
    agent_id: &str,
) -> anyhow::Result<Option<Arc<GatewayStore>>> {
    let gateway_dir = agents_dir.join(".gateway");
    std::fs::create_dir_all(&gateway_dir)?;
    let store = Arc::new(GatewayStore::open(&gateway_dir)?);
    seed_agent_revision(&store, config, agent_id, &agents_dir.join(agent_id))?;
    Ok(Some(store))
}

fn is_bwrap_unavailable(e: &anyhow::Error) -> bool {
    let msg = e.to_string();
    msg.contains("bwrap") || msg.contains("bubblewrap")
}

#[tokio::test]
async fn middleware_maps_both_sides_of_the_script_boundary() -> anyhow::Result<()> {
    let workspace = TestWorkspace::new()?;
    let config = workspace.gateway_config();
    let agent_id = "script-mapping-agent";
    install_mapping_agent(&workspace.agents_dir.join(agent_id), agent_id)?;
    let store = setup_store(&config, &workspace.agents_dir, agent_id)?;

    let execution = GatewayExecutionService::new(config, store);
    let result = execution
        .spawn_agent_once(
            agent_id,
            r#"{"location":"Paris"}"#,
            "session-script-middleware-map",
            None,
            false,
            None,
            None,
            None,
            None,
            None,
            &[],
        )
        .await;

    match result {
        Ok(spawn_result) => {
            let reply = spawn_result.assistant_reply.expect("should have reply");
            // post_process replaced the entrypoint's `summary` shape with the
            // caller's `result` shape; the reply must carry ONLY the mapped
            // output, proving the hook ran after the entrypoint.
            assert!(
                reply.contains("\"result\"") && reply.contains("forecast:Paris"),
                "reply should be the post-mapped output, got: {reply}"
            );
            assert!(
                !reply.contains("summary"),
                "raw entrypoint stdout must not leak past post_process, got: {reply}"
            );
        }
        Err(e) => {
            if is_bwrap_unavailable(&e) {
                eprintln!("skipping middleware_maps_both_sides_of_the_script_boundary: bwrap not available");
                return Ok(());
            }
            return Err(e);
        }
    }
    Ok(())
}

#[tokio::test]
async fn broken_pre_hook_fails_the_turn_without_running_the_entrypoint() -> anyhow::Result<()> {
    let workspace = TestWorkspace::new()?;
    let config = workspace.gateway_config();
    let agent_id = "script-broken-hook-agent";
    install_failing_hook_agent(&workspace.agents_dir.join(agent_id), agent_id)?;
    let store = setup_store(&config, &workspace.agents_dir, agent_id)?;

    let execution = GatewayExecutionService::new(config, store);
    let result = execution
        .spawn_agent_once(
            agent_id,
            r#"{"location":"Paris"}"#,
            "session-script-middleware-fail",
            None,
            false,
            None,
            None,
            None,
            None,
            None,
            &[],
        )
        .await;

    match result {
        Ok(_) => anyhow::bail!(
            "a failing pre_process hook must fail the turn, not fall through to the entrypoint"
        ),
        Err(e) => {
            if is_bwrap_unavailable(&e) {
                eprintln!("skipping broken_pre_hook_fails_the_turn_without_running_the_entrypoint: bwrap not available");
                return Ok(());
            }
            let msg = e.to_string();
            assert!(
                msg.contains("pre_process"),
                "error should name the failed hook, got: {msg}"
            );
        }
    }
    Ok(())
}

/// The full #1251 cheap path with the real generator: schema_diff →
/// generate_wrapper --wrapper-mode script → install → spawn. The reply must
/// be the deterministic mapped output of the copied base script — no LLM
/// anywhere in the turn.
#[tokio::test]
async fn generated_script_wrapper_runs_end_to_end_without_an_llm() -> anyhow::Result<()> {
    let adapter_scripts = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("agents")
        .join("evolution")
        .join("agent-adapter.default")
        .join("scripts");

    // The base speaks `{city} -> {summary}`; the caller wants `{location} ->
    // {result}`. The base entry is a deterministic script.
    let workspace = TestWorkspace::new()?;
    let wrapper_id = "generated-script-wrapper";
    let wrapper_dir = workspace.agents_dir.join(wrapper_id);
    let base_entry = workspace.path().join("base_weather.py");
    std::fs::write(
        &base_entry,
        r#"#!/usr/bin/env python3
import json, sys
payload = json.load(sys.stdin)
print(json.dumps({"summary": f"forecast:{payload['city']}"}))
"#,
    )?;

    let diff_input = serde_json::json!({
        "base_accepts": {"type": "object", "required": ["city"], "properties": {"city": {"type": "string"}}},
        "base_returns": {"type": "object", "required": ["summary"], "properties": {"summary": {"type": "string"}}},
        "target_accepts": {"type": "object", "required": ["location"], "properties": {"location": {"type": "string"}}},
        "target_returns": {"type": "object", "required": ["result"], "properties": {"result": {"type": "string"}}}
    });
    let diff = {
        use std::process::{Command, Stdio};
        let mut child = Command::new("python3")
            .arg(adapter_scripts.join("schema_diff.py"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()?;
        use std::io::Write;
        child
            .stdin
            .take()
            .expect("stdin")
            .write_all(serde_json::to_string(&diff_input).unwrap().as_bytes())?;
        let out = child.wait_with_output()?;
        assert!(out.status.success());
        serde_json::from_slice::<serde_json::Value>(&out.stdout)?
    };

    let target_spec = serde_json::json!({
        "accepts": diff_input["target_accepts"],
        "returns": diff_input["target_returns"]
    });
    let base_schemas = serde_json::json!({
        "accepts": diff_input["base_accepts"],
        "returns": diff_input["base_returns"]
    });
    let gen = Command::new("python3")
        .arg(adapter_scripts.join("generate_wrapper.py"))
        .arg("--base-skill")
        .arg({
            let p = workspace.path().join("base-skill.md");
            std::fs::write(&p, "---\nname: \"base.weather\"\n---\n# Weather\n")?;
            p
        }.to_string_lossy().to_string())
        .arg("--base-agent-id")
        .arg("base.weather")
        .arg("--wrapper-id")
        .arg(wrapper_id)
        .arg("--target-spec-json")
        .arg(serde_json::to_string(&target_spec).unwrap())
        .arg("--schema-diff-json")
        .arg(serde_json::to_string(&diff).unwrap())
        .arg("--base-schema-json")
        .arg(serde_json::to_string(&base_schemas).unwrap())
        .arg("--base-manifest-json")
        .arg(r#"{"capabilities": []}"#)
        .arg("--base-revision-digest")
        .arg("rev_sha256:testbase")
        .arg("--wrapper-mode")
        .arg("script")
        .arg("--base-script-path")
        .arg(base_entry.to_string_lossy().to_string())
        .arg("--output-dir")
        .arg(wrapper_dir.to_string_lossy().to_string())
        .output()?;
    assert!(
        gen.status.success(),
        "generate_wrapper.py failed: {}",
        String::from_utf8_lossy(&gen.stderr)
    );
    let gen_json: serde_json::Value = serde_json::from_slice(&gen.stdout)?;
    assert_eq!(
        gen_json["verdict"], "ok",
        "the flat rename should be proven by the round-trip: {gen_json}"
    );

    let config = workspace.gateway_config();
    let store = setup_store(&config, &workspace.agents_dir, wrapper_id)?;

    let execution = GatewayExecutionService::new(config, store);
    let result = execution
        .spawn_agent_once(
            wrapper_id,
            r#"{"location":"Paris"}"#,
            "session-generated-script-wrapper",
            None,
            false,
            None,
            None,
            None,
            None,
            None,
            &[],
        )
        .await;

    match result {
        Ok(spawn_result) => {
            let reply = spawn_result.assistant_reply.expect("should have reply");
            assert!(
                reply.contains("forecast:Paris") && reply.contains("\"result\""),
                "reply should be the target-shape mapped output, got: {reply}"
            );
            assert!(
                !reply.contains("summary") && !reply.contains("city"),
                "base-shape fields must not leak past the mapping hooks, got: {reply}"
            );
        }
        Err(e) => {
            if is_bwrap_unavailable(&e) {
                eprintln!("skipping generated_script_wrapper_runs_end_to_end_without_an_llm: bwrap not available");
                return Ok(());
            }
            return Err(e);
        }
    }
    Ok(())
}
