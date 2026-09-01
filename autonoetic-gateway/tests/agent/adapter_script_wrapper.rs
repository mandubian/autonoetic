//! Script-mode wrapper generation (#1251): `--wrapper-mode script` emits a
//! deterministic wrapper — an entry shim running an in-bundle copy of the
//! base's script entry, with verbatim stdin->stdout mapping hooks. The
//! wrapper never pays for a completion; these tests pin the bundle shape,
//! the manifest contract, the hook behavior, and the mechanical refusals.

use autonoetic_gateway::runtime::parser::SkillParser;
use autonoetic_types::agent::ExecutionMode;
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

struct GenOutput {
    status: std::process::ExitStatus,
    stdout: serde_json::Value,
    stderr: String,
}

impl std::fmt::Display for GenOutput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "status={} stdout={} stderr={}",
            self.status,
            serde_json::to_string_pretty(&self.stdout).unwrap_or_default(),
            self.stderr
        )
    }
}

fn run_generator(temp: &tempfile::TempDir, wrapper_id: &str, extra_args: &[&str]) -> GenOutput {
    let base_skill_path = temp.path().join("base.SKILL.md");
    if !base_skill_path.exists() {
        std::fs::write(
            &base_skill_path,
            "---\nname: \"base.agent\"\ndescription: \"base\"\n---\n# Base\n",
        )
        .expect("base skill should write");
    }
    let out = Command::new("python3")
        .arg(script_path("generate_wrapper.py"))
        .arg("--base-skill")
        .arg(base_skill_path.to_string_lossy().to_string())
        .arg("--base-agent-id")
        .arg("base.agent")
        .arg("--wrapper-id")
        .arg(wrapper_id)
        .args(extra_args)
        .output()
        .expect("generate_wrapper.py should execute");
    let stdout = if out.stdout.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_slice(&out.stdout).expect("generator stdout should be json")
    };
    GenOutput {
        status: out.status,
        stdout,
        stderr: String::from_utf8_lossy(&out.stderr).to_string(),
    }
}


struct Scenario {
    temp: tempfile::TempDir,
}

impl Scenario {
    fn new() -> Self {
        let temp = tempfile::tempdir().expect("tempdir");
        let entry = temp.path().join("base_weather.py");
        std::fs::write(
            &entry,
            r#"#!/usr/bin/env python3
import json, sys
payload = json.load(sys.stdin)
print(json.dumps({"summary": f"forecast:{payload['city']}"}))
"#,
        )
        .expect("base entry write");
        Self { temp }
    }

    fn generate(&self, wrapper_id: &str, extra_args: &[&str]) -> GenOutput {
        let base_script = self.temp.path().join("base_weather.py");
        let base_script_str = base_script.to_string_lossy().into_owned();
        let diff = serde_json::json!({
            "accepts_compatible": false,
            "returns_compatible": false,
            "requires_input_mapping": true,
            "requires_output_mapping": true,
            "input_mappings": [{"from": "location", "to": "city"}],
            "output_mappings": [{"from": "result", "to": "summary"}],
            "notes": []
        });
        let diff_str = serde_json::to_string(&diff).unwrap();
        let mut args: Vec<&str> = vec![
            "--target-spec-json",
            r#"{"accepts":{"type":"object","required":["location"],"properties":{"location":{"type":"string"}}},"returns":{"type":"object","required":["result"],"properties":{"result":{"type":"string"}}}}"#,
            "--schema-diff-json",
            diff_str.as_str(),
            "--base-schema-json",
            r#"{"accepts":{"type":"object","required":["city"],"properties":{"city":{"type":"string"}}},"returns":{"type":"object","required":["summary"],"properties":{"summary":{"type":"string"}}}}"#,
            "--base-manifest-json",
            r#"{"capabilities": [], "script_input_mode": "stdin"}"#,
            "--wrapper-mode",
            "script",
            "--base-script-path",
            base_script_str.as_str(),
        ];
        args.extend_from_slice(extra_args);
        run_generator(&self.temp, wrapper_id, &args)
    }
}

#[test]
fn script_mode_wrapper_generates_deterministic_bundle() {
    let scenario = Scenario::new();
    let out_dir = scenario.temp.path().join("wrapper-script");
    let gen = scenario.generate(
        "base.agent.scriptwrap",
        &[
            "--base-revision-digest",
            "rev_sha256:feed",
            "--output-dir",
            out_dir.to_string_lossy().as_ref(),
        ],
    );
    assert!(gen.status.success(), "generator failed: {}", gen.stderr);
    assert_eq!(gen.stdout["verdict"], "ok", "{gen:#}");
    assert_eq!(gen.stdout["wrapper_mode"], "script");

    for expected in [
        "SKILL.md",
        "runtime.lock",
        "scripts/entry.py",
        "scripts/base_entry.py",
        "scripts/pre_map.py",
        "scripts/post_map.py",
    ] {
        assert!(
            out_dir.join(expected).exists(),
            "script-mode bundle should contain {expected}"
        );
    }
    // The pinned copy ships the base's entry verbatim.
    let copied = std::fs::read_to_string(out_dir.join("scripts/base_entry.py")).unwrap();
    assert!(
        copied.contains("forecast:"),
        "base_entry.py should be the base's script, got:\n{copied}"
    );
    let shim = std::fs::read_to_string(out_dir.join("scripts/entry.py")).unwrap();
    assert!(
        shim.contains("base_entry.py") && shim.contains("runpy"),
        "entry shim should run the copy in-process, got:\n{shim}"
    );

    // The manifest must parse as a genuine script-mode agent with middleware,
    // inherited input mode, and the digest-pinned provenance.
    let skill = std::fs::read_to_string(out_dir.join("SKILL.md")).expect("SKILL.md readable");
    let (manifest, _) = SkillParser::parse(&skill).expect("wrapper should parse");
    assert_eq!(manifest.execution_mode, ExecutionMode::Script);
    assert_eq!(manifest.script_entry.as_deref(), Some("scripts/entry.py"));
    let middleware = manifest
        .middleware
        .expect("script wrapper should declare middleware");
    assert_eq!(
        middleware.pre_process.as_deref(),
        Some("python3 scripts/pre_map.py")
    );
    assert_eq!(
        middleware.post_process.as_deref(),
        Some("python3 scripts/post_map.py")
    );
    let adapter = manifest
        .adapter
        .expect("provenance should parse — drift detection depends on it");
    assert_eq!(adapter.base_agent_id, "base.agent");
    assert_eq!(adapter.base_revision_digest.as_deref(), Some("rev_sha256:feed"));
    // A script wrapper never reasons: no preset, no inline provider config.
    assert!(
        manifest.llm_preset.is_none() && manifest.llm_config.is_none(),
        "script wrapper must not carry inference settings"
    );
    assert!(
        skill.contains("script_input_mode: \"stdin\""),
        "the base's input delivery mode should be inherited, got:\n{skill}"
    );
}

#[test]
fn script_mode_hooks_map_verbatim_payloads() {
    let scenario = Scenario::new();
    let out_dir = scenario.temp.path().join("wrapper-hooks");
    let gen = scenario.generate(
        "base.agent.scripthooks",
        &["--output-dir", out_dir.to_string_lossy().as_ref()],
    );
    assert!(gen.status.success(), "generator failed: {}", gen.stderr);

    let run_hook = |script: &Path, stdin: &str| {
        let mut child = Command::new("python3")
            .arg(script)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("hook should spawn");
        use std::io::Write;
        child
            .stdin
            .take()
            .expect("stdin available")
            .write_all(stdin.as_bytes())
            .expect("stdin write");
        let out = child.wait_with_output().expect("hook should run");
        assert!(
            out.status.success(),
            "hook failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice::<serde_json::Value>(&out.stdout).expect("hook stdout json")
    };

    // Verbatim contract: raw payload in, mapped payload out — no envelope.
    let pre = run_hook(
        &out_dir.join("scripts/pre_map.py"),
        r#"{"location": "Paris"}"#,
    );
    assert_eq!(pre, serde_json::json!({ "city": "Paris" }));

    let post = run_hook(
        &out_dir.join("scripts/post_map.py"),
        r#"{"summary": "forecast:Paris"}"#,
    );
    assert_eq!(post, serde_json::json!({ "result": "forecast:Paris" }));
}

#[test]
fn script_mode_refuses_unwrappable_bases() {
    let scenario = Scenario::new();
    let target_spec = r#"{"accepts":{"type":"object","required":["location"],"properties":{"location":{"type":"string"}}}}"#;

    // Missing --base-script-path entirely.
    let gen = run_generator(
        &scenario.temp,
        "base.agent.refuse.noflag",
        &[
            "--target-spec-json",
            target_spec,
            "--schema-diff-json",
            r#"{"requires_input_mapping":false,"requires_output_mapping":false,"input_mappings":[],"output_mappings":[],"notes":[]}"#,
            "--wrapper-mode",
            "script",
        ],
    );
    assert_eq!(
        gen.status.code(),
        Some(2),
        "missing base script must refuse loudly: {gen:#}"
    );
    assert_eq!(gen.stdout["verdict"], "clarification_needed");
    assert!(
        gen.stdout["error"].as_str().unwrap().contains("--base-script-path"),
        "the error should name the missing flag: {gen:#}"
    );

    // A shell entry — the Python-only shim cannot run it.
    let shell = scenario.temp.path().join("entry.sh");
    std::fs::write(&shell, "echo hi\n").expect("shell write");
    let gen = run_generator(
        &scenario.temp,
        "base.agent.refuse.shell",
        &[
            "--target-spec-json",
            target_spec,
            "--schema-diff-json",
            r#"{"requires_input_mapping":false,"requires_output_mapping":false,"input_mappings":[],"output_mappings":[],"notes":[]}"#,
            "--wrapper-mode",
            "script",
            "--base-script-path",
            shell.to_string_lossy().as_ref(),
        ],
    );
    assert_eq!(gen.status.code(), Some(2));
    assert_eq!(gen.stdout["verdict"], "clarification_needed");

    // A Python entry that does not compile would fail every turn — refuse.
    let broken = scenario.temp.path().join("broken.py");
    std::fs::write(&broken, "def broken(:\n").expect("broken write");
    let gen = run_generator(
        &scenario.temp,
        "base.agent.refuse.broken",
        &[
            "--target-spec-json",
            target_spec,
            "--schema-diff-json",
            r#"{"requires_input_mapping":false,"requires_output_mapping":false,"input_mappings":[],"output_mappings":[],"notes":[]}"#,
            "--wrapper-mode",
            "script",
            "--base-script-path",
            broken.to_string_lossy().as_ref(),
        ],
    );
    assert_eq!(gen.status.code(), Some(2));
    assert!(gen.stdout["error"].as_str().unwrap().contains("compile"));

    // Script agents require an object io.accepts — the generator refuses
    // earlier with the same verdict instead of shipping an uninstallable
    // bundle.
    let entry = scenario.temp.path().join("base_weather.py");
    let gen = run_generator(
        &scenario.temp,
        "base.agent.refuse.noio",
        &[
            "--target-spec-json",
            r#"{}"#,
            "--schema-diff-json",
            r#"{"requires_input_mapping":false,"requires_output_mapping":false,"input_mappings":[],"output_mappings":[],"notes":[]}"#,
            "--wrapper-mode",
            "script",
            "--base-script-path",
            entry.to_string_lossy().as_ref(),
        ],
    );
    assert_eq!(gen.status.code(), Some(2));
    assert!(
        gen.stdout["error"].as_str().unwrap().contains("io.accepts"),
        "the error should name the missing input contract: {gen:#}"
    );
}

#[test]
fn script_mode_without_mapping_is_a_pure_rebundle() {
    let scenario = Scenario::new();
    let entry = scenario.temp.path().join("base_weather.py");
    let schema = r#"{"type":"object","required":["city"],"properties":{"city":{"type":"string"}}}"#;
    let target_spec = format!(r#"{{"accepts":{schema}}}"#);
    let gen = run_generator(
        &scenario.temp,
        "base.agent.rebundle",
        &[
            "--target-spec-json",
            target_spec.as_str(),
            "--schema-diff-json",
            r#"{"accepts_compatible":true,"returns_compatible":true,"requires_input_mapping":false,"requires_output_mapping":false,"input_mappings":[],"output_mappings":[],"notes":[]}"#,
            "--wrapper-mode",
            "script",
            "--base-script-path",
            entry.to_string_lossy().as_ref(),
        ],
    );
    assert!(gen.status.success(), "generator failed: {}", gen.stderr);
    assert_eq!(gen.stdout["verdict"], "ok", "{gen:#}");
    let files: Vec<String> = gen.stdout["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    assert!(
        !files.iter().any(|f| f.contains("_map")),
        "identical schema needs no mapper: {files:?}"
    );
    // The shim + copy are still emitted — the wrapper must remain runnable.
    assert!(files.iter().any(|f| f == "scripts/entry.py"));
    assert!(files.iter().any(|f| f == "scripts/base_entry.py"));
}
