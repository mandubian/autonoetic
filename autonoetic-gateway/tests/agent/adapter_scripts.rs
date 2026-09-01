use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

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

#[test]
fn test_adapter_scripts_generate_wrapper_with_mapping_hooks() {
    let schema_diff_script = script_path("schema_diff.py");
    let generate_wrapper_script = script_path("generate_wrapper.py");
    assert!(schema_diff_script.exists(), "schema_diff.py should exist");
    assert!(
        generate_wrapper_script.exists(),
        "generate_wrapper.py should exist"
    );

    let diff_input = serde_json::json!({
        "base_accepts": {
            "type": "object",
            "required": ["query"],
            "properties": { "query": { "type": "string" } }
        },
        "base_returns": {
            "type": "object",
            "required": ["summary"],
            "properties": { "summary": { "type": "string" } }
        },
        "target_accepts": {
            "type": "object",
            "required": ["task"],
            "properties": { "task": { "type": "string" } }
        },
        "target_returns": {
            "type": "object",
            "required": ["result"],
            "properties": { "result": { "type": "string" } }
        }
    });
    let diff = run_python_with_stdin(&schema_diff_script, &[], &diff_input);
    assert_eq!(
        diff["requires_input_mapping"],
        serde_json::Value::Bool(true)
    );
    assert_eq!(
        diff["requires_output_mapping"],
        serde_json::Value::Bool(true)
    );

    let temp = tempfile::tempdir().expect("tempdir should create");
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

    let output_dir = temp.path().join("wrapper");
    let out = Command::new("python3")
        .arg(&generate_wrapper_script)
        .arg("--base-skill")
        .arg(base_skill_path.to_string_lossy().to_string())
        .arg("--base-agent-id")
        .arg("base.agent")
        .arg("--wrapper-id")
        .arg("base.agent.adapter")
        .arg("--target-spec-json")
        .arg(serde_json::to_string(&target_spec).expect("target spec serializes"))
        .arg("--schema-diff-json")
        .arg(serde_json::to_string(&diff).expect("diff serializes"))
        .arg("--output-dir")
        .arg(output_dir.to_string_lossy().to_string())
        .output()
        .expect("generator should execute");

    assert!(
        out.status.success(),
        "generate_wrapper.py failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let generated: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("generator stdout should be json");
    assert_eq!(generated["wrapper_id"], "base.agent.adapter");
    assert_eq!(
        generated["requires_input_mapping"],
        serde_json::Value::Bool(true)
    );
    assert_eq!(
        generated["requires_output_mapping"],
        serde_json::Value::Bool(true)
    );

    assert!(output_dir.join("SKILL.md").exists());
    assert!(output_dir.join("scripts").join("pre_map.py").exists());
    assert!(output_dir.join("scripts").join("post_map.py").exists());

    let skill_md = std::fs::read_to_string(output_dir.join("SKILL.md")).expect("SKILL.md readable");
    assert!(
        skill_md.contains("base_agent_id: \"base.agent\""),
        "should have base_agent_id traceability"
    );
    assert!(
        skill_md.contains("generated_at:"),
        "should have generation timestamp"
    );
}

/// A wrapper must reason with the *base agent's* model. The generator used to
/// hardcode `provider: openai` / `model: gpt-4o` into every wrapper, which
/// would pin a stale model regardless of gateway config or base agent (#818
/// adapter prerequisite).
#[test]
fn test_generated_wrapper_inherits_base_inference_without_hardcoding_a_model() {
    let generate_wrapper_script = script_path("generate_wrapper.py");
    let temp = tempfile::tempdir().expect("tempdir should create");
    let base_skill_path = temp.path().join("base.SKILL.md");
    std::fs::write(&base_skill_path, "---\nname: \"base.agent\"\n---\n# Base\n")
        .expect("base skill should write");

    let render = |base_manifest: &serde_json::Value, out: &str| -> String {
        let output_dir = temp.path().join(out);
        let result = Command::new("python3")
            .arg(&generate_wrapper_script)
            .arg("--base-skill")
            .arg(base_skill_path.to_string_lossy().to_string())
            .arg("--base-agent-id")
            .arg("base.agent")
            .arg("--wrapper-id")
            .arg("base.agent.adapter")
            .arg("--target-spec-json")
            .arg("{}")
            .arg("--schema-diff-json")
            .arg("{}")
            .arg("--base-manifest-json")
            .arg(serde_json::to_string(base_manifest).expect("manifest serializes"))
            .arg("--output-dir")
            .arg(output_dir.to_string_lossy().to_string())
            .output()
            .expect("generator should execute");
        assert!(
            result.status.success(),
            "generate_wrapper.py failed: {}",
            String::from_utf8_lossy(&result.stderr)
        );
        std::fs::read_to_string(output_dir.join("SKILL.md")).expect("SKILL.md readable")
    };

    // Base declares a preset → wrapper reuses it, so the gateway resolves
    // provider/model from its own config.
    let from_preset = render(&serde_json::json!({ "llm_preset": "coding" }), "preset");
    assert!(
        from_preset.contains("llm_preset: \"coding\""),
        "wrapper should inherit the base preset, got:\n{from_preset}"
    );
    assert!(
        !from_preset.contains("gpt-4o") && !from_preset.contains("provider:"),
        "wrapper must not name a hardcoded provider/model, got:\n{from_preset}"
    );

    // Base pins an explicit config → wrapper copies it, but pins temperature to
    // 0.0 because it is a deterministic transformation layer.
    let from_config = render(
        &serde_json::json!({
            "llm_config": { "provider": "anthropic", "model": "claude-sonnet-5", "temperature": 0.7 }
        }),
        "config",
    );
    assert!(from_config.contains("\"provider\": \"anthropic\""));
    assert!(from_config.contains("\"model\": \"claude-sonnet-5\""));
    assert!(
        from_config.contains("\"temperature\": 0.0"),
        "wrapper should pin temperature 0.0, got:\n{from_config}"
    );

    // No inference info at all → a preset name, never a provider guess.
    let fallback = render(&serde_json::json!({}), "fallback");
    assert!(fallback.contains("llm_preset: \"agentic\""));
    assert!(!fallback.contains("gpt-4o"));
}

#[test]
fn test_schema_diff_emits_multiple_mappings() {
    let schema_diff_script = script_path("schema_diff.py");
    let diff_input = serde_json::json!({
        "base_accepts": {
            "type": "object",
            "required": ["query", "domain"]
        },
        "base_returns": {
            "type": "object",
            "required": ["summary", "confidence"]
        },
        "target_accepts": {
            "type": "object",
            "required": ["task", "topic"]
        },
        "target_returns": {
            "type": "object",
            "required": ["result", "score"]
        }
    });

    let diff = run_python_with_stdin(&schema_diff_script, &[], &diff_input);
    let input_mappings = diff
        .get("input_mappings")
        .and_then(|v| v.as_array())
        .expect("input_mappings should be array");
    let output_mappings = diff
        .get("output_mappings")
        .and_then(|v| v.as_array())
        .expect("output_mappings should be array");

    assert_eq!(input_mappings.len(), 2);
    assert_eq!(output_mappings.len(), 2);
}

/// The drift machinery (roster `stale_base`, spawn advisories, promotion-time
/// drift events) only fires when wrapper provenance claims a digest — and only
/// the adapter agent can supply it at generation time, via its SKILL.md
/// instructions. Pin the wiring: if the instruction is renamed away, this
/// fails instead of every future wrapper silently under-claiming (#1221).
#[test]
fn adapter_skill_md_instructs_base_revision_digest_capture() {
    let skill_path = script_path("../SKILL.md");
    let skill = std::fs::read_to_string(&skill_path)
        .expect("agent-adapter.default SKILL.md should exist");

    assert!(
        skill.contains("--base-revision-digest"),
        "SKILL.md must instruct passing the base revision digest to \
         generate_wrapper.py — without it every generated wrapper under-claims \
         and drift detection stays dormant"
    );
    assert!(
        skill.contains("agent_inspect"),
        "SKILL.md must instruct calling agent_inspect to learn the base's \
         promoted revision"
    );
}

/// The generator's round-trip verdict (#1234) is mechanical; the adapter's
/// io.returns.status must mirror it, not override it with LLM judgment. If
/// the instruction disappears from the SKILL.md, adapters regress to claiming
/// `ok` for under-proven mappings — pin the wiring the same way the digest
/// capture is pinned.
#[test]
fn adapter_skill_md_pins_mechanical_verdict_to_reported_status() {
    let skill_path = script_path("../SKILL.md");
    let skill = std::fs::read_to_string(&skill_path)
        .expect("agent-adapter.default SKILL.md should exist");

    assert!(
        skill.contains("--base-schema-json"),
        "SKILL.md must instruct passing the base's I/O schemas to \
         generate_wrapper.py — without them the round-trip proof is skipped \
         and every verdict under-claims"
    );
    for verdict in ["partial", "clarification_needed"] {
        assert!(
            skill.contains(&format!("\"{verdict}\"")),
            "SKILL.md must explain the generator's `{verdict}` verdict and the \
             status it maps to — without it an adapter can claim `ok` for a \
             mapping the generator could not prove"
        );
    }
    assert!(
        skill.contains("--wrapper-mode script"),
        "SKILL.md must instruct generating script-mode wrappers for script \
         bases (#1251) — the deterministic wrapper never pays for a completion"
    );
}
