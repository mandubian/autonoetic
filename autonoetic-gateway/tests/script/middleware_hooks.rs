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
