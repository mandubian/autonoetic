//! Round-trip validation of generated adapter mappings (#1234): the generator
//! executes every mapper it emits against a synthetic payload and validates
//! the output against the other side's schema. The verdict in its stdout JSON
//! is mechanical — these tests pin what each schema shape must produce, so a
//! wrong mapping can never ship claimed as `ok`.

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

fn run_schema_diff(payload: &serde_json::Value) -> serde_json::Value {
    let mut child = Command::new("python3")
        .arg(script_path("schema_diff.py"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("schema_diff.py should spawn");
    use std::io::Write;
    child
        .stdin
        .take()
        .expect("stdin available")
        .write_all(serde_json::to_string(payload).unwrap().as_bytes())
        .expect("stdin write");
    let out = child.wait_with_output().expect("schema_diff.py should run");
    assert!(
        out.status.success(),
        "schema_diff.py failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).expect("schema_diff stdout should be json")
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

/// Run the generator with the standard adapter invocation; callers add
/// scenario-specific flags.
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

fn string_object_schema(required: &[&str]) -> serde_json::Value {
    let properties: serde_json::Map<String, serde_json::Value> = required
        .iter()
        .map(|name| (name.to_string(), serde_json::json!({ "type": "string" })))
        .collect();
    serde_json::json!({ "type": "object", "required": required, "properties": properties })
}

fn flat_schemas() -> serde_json::Value {
    serde_json::json!({
        "base_accepts": string_object_schema(&["query", "domain"]),
        "base_returns": string_object_schema(&["summary", "confidence"]),
        "target_accepts": string_object_schema(&["task", "topic"]),
        "target_returns": string_object_schema(&["result", "score"])
    })
}

fn write_base_entry(temp: &tempfile::TempDir, name: &str) -> String {
    let entry = temp.path().join(name);
    std::fs::write(&entry, "print('unused')\n").expect("base entry write");
    entry.to_string_lossy().into_owned()
}

/// A proven flat rename must be reported `ok` — the whole point of the
/// round-trip proof — and the provenance notes must record the proof.
#[test]
fn roundtrip_proven_flat_rename_reports_ok() {
    let temp = tempfile::tempdir().expect("tempdir");
    let schemas = flat_schemas();
    let diff = run_schema_diff(&schemas);
    let out_dir = temp.path().join("wrapper-ok");
    let gen = run_generator(
        &temp,
        "base.agent.adapter.ok",
        &[
            "--target-spec-json",
            &serde_json::to_string(
                &serde_json::json!({
                    "accepts": schemas["target_accepts"],
                    "returns": schemas["target_returns"]
                }),
            )
            .unwrap(),
            "--schema-diff-json",
            &serde_json::to_string(&diff).unwrap(),
            "--base-schema-json",
            &serde_json::to_string(
                &serde_json::json!({
                    "accepts": schemas["base_accepts"],
                    "returns": schemas["base_returns"]
                }),
            )
            .unwrap(),
            "--output-dir",
            out_dir.to_string_lossy().as_ref(),
        ],
    );
    assert!(
        gen.status.success(),
        "generator failed: {}",
        gen.stderr
    );
    assert_eq!(gen.stdout["verdict"], "ok", "flat rename should prove: {gen:#}");
    assert!(gen.stdout["validation_failures"].as_array().unwrap().is_empty());

    let skill = std::fs::read_to_string(out_dir.join("SKILL.md")).expect("SKILL.md readable");
    assert!(
        skill.contains("round-trip validation passed"),
        "provenance notes should record the round-trip proof, got:\n{skill}"
    );
}

/// The required-field-order guess (`target_required[i] -> base_required[i]`)
/// pairing a string with an integer must be *named* — the verdict degrades to
/// `partial` and the failing path appears in validation_failures instead of a
/// confidently wrong mapper shipping as `ok`.
#[test]
fn wrong_order_pairing_across_types_is_named_not_claimed_ok() {
    let temp = tempfile::tempdir().expect("tempdir");
    let schemas = serde_json::json!({
        "base_accepts": serde_json::json!({
            "type": "object",
            "required": ["query", "limit"],
            "properties": {
                "query": { "type": "string" },
                "limit": { "type": "integer" }
            }
        }),
        "base_returns": string_object_schema(&["summary"]),
        "target_accepts": string_object_schema(&["task", "topic"]),
        "target_returns": string_object_schema(&["result"])
    });
    let diff = run_schema_diff(&schemas);
    let gen = run_generator(
        &temp,
        "base.agent.adapter.typegap",
        &[
            "--target-spec-json",
            &serde_json::to_string(
                &serde_json::json!({
                    "accepts": schemas["target_accepts"],
                    "returns": schemas["target_returns"]
                }),
            )
            .unwrap(),
            "--schema-diff-json",
            &serde_json::to_string(&diff).unwrap(),
            "--base-schema-json",
            &serde_json::to_string(
                &serde_json::json!({
                    "accepts": schemas["base_accepts"],
                    "returns": schemas["base_returns"]
                }),
            )
            .unwrap(),
        ],
    );
    assert!(gen.status.success(), "generator failed: {}", gen.stderr);
    assert_eq!(
        gen.stdout["verdict"], "partial",
        "a type-gap pairing must under-claim, not claim ok: {gen:#}"
    );
    let failures: Vec<String> = gen.stdout["validation_failures"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    assert!(
        failures.iter().any(|f| f.contains("limit")),
        "the unmappable `limit` field must be named in validation_failures: {failures:?}"
    );
    let notes: Vec<String> = gen.stdout["notes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    assert!(
        notes.iter().any(|n| n.contains("dropped rename topic->limit")),
        "the type-invalid rename must be dropped and named: {notes:?}"
    );
}

/// Missing schema on one side means no mapper is derivable: no passthrough
/// hook masquerading as an adapter — the verdict is `clarification_needed`
/// and the hook file must not exist.
#[test]
fn missing_base_schema_refuses_mapper_with_clarification() {
    let temp = tempfile::tempdir().expect("tempdir");
    let diff = run_schema_diff(&serde_json::json!({
        "base_accepts": null,
        "base_returns": null,
        "target_accepts": string_object_schema(&["task"]),
        "target_returns": string_object_schema(&["result"])
    }));
    let out_dir = temp.path().join("wrapper-miss");
    let gen = run_generator(
        &temp,
        "base.agent.adapter.miss",
        &[
            "--target-spec-json",
            &serde_json::to_string(
                &serde_json::json!({
                    "accepts": string_object_schema(&["task"]),
                    "returns": string_object_schema(&["result"])
                }),
            )
            .unwrap(),
            "--schema-diff-json",
            &serde_json::to_string(&diff).unwrap(),
            "--output-dir",
            out_dir.to_string_lossy().as_ref(),
        ],
    );
    assert!(gen.status.success(), "generator failed: {}", gen.stderr);
    assert_eq!(gen.stdout["verdict"], "clarification_needed", "{gen:#}");
    let files: Vec<String> = gen.stdout["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    assert!(
        !files.iter().any(|f| f.contains("pre_map")),
        "no mapper should be emitted without a schema to map against: {files:?}"
    );
    assert!(
        !out_dir.join("scripts/pre_map.py").exists(),
        "pre_map.py must not be written for a refused mapping"
    );
}

/// Without `--base-schema-json` the round-trip cannot run — the verdict must
/// under-claim to `partial` (never guess), with a note saying validation was
/// skipped.
#[test]
fn roundtrip_without_base_schemas_under_claims_to_partial() {
    let temp = tempfile::tempdir().expect("tempdir");
    let schemas = flat_schemas();
    let diff = run_schema_diff(&schemas);
    let gen = run_generator(
        &temp,
        "base.agent.adapter.underclaim",
        &[
            "--target-spec-json",
            &serde_json::to_string(
                &serde_json::json!({
                    "accepts": schemas["target_accepts"],
                    "returns": schemas["target_returns"]
                }),
            )
            .unwrap(),
            "--schema-diff-json",
            &serde_json::to_string(&diff).unwrap(),
        ],
    );
    assert!(gen.status.success(), "generator failed: {}", gen.stderr);
    assert_eq!(
        gen.stdout["verdict"], "partial",
        "an unproven mapping must under-claim: {gen:#}"
    );
    let notes: Vec<String> = gen.stdout["notes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    assert!(
        notes.iter().any(|n| n.contains("round-trip validation skipped")),
        "the skip must be recorded in notes: {notes:?}"
    );
}

/// Enum values the rename cannot translate are a mechanical validation
/// failure with a named path — partial, never ok. The returns direction is
/// schema-identical (no mapping needed) so the enum gap is the only defect.
#[test]
fn enum_mismatch_fails_roundtrip_with_named_path() {
    let temp = tempfile::tempdir().expect("tempdir");
    let plain = serde_json::json!({ "type": "object", "required": ["extra"] });
    let diff = run_schema_diff(&serde_json::json!({
        "base_accepts": serde_json::json!({
            "type": "object",
            "required": ["query"],
            "properties": { "query": { "type": "string", "enum": ["x"] } }
        }),
        "base_returns": plain,
        "target_accepts": serde_json::json!({
            "type": "object",
            "required": ["task"],
            "properties": { "task": { "type": "string", "enum": ["a"] } }
        }),
        "target_returns": plain
    }));
    let gen = run_generator(
        &temp,
        "base.agent.adapter.enum",
        &[
            "--target-spec-json",
            r#"{"accepts":{"type":"object","required":["task"],"properties":{"task":{"type":"string","enum":["a"]}}},"returns":{"type":"object","required":["extra"]}}"#,
            "--schema-diff-json",
            &serde_json::to_string(&diff).unwrap(),
            "--base-schema-json",
            r#"{"accepts":{"type":"object","required":["query"],"properties":{"query":{"type":"string","enum":["x"]}}},"returns":{"type":"object","required":["extra"]}}"#,
        ],
    );
    assert!(gen.status.success(), "generator failed: {}", gen.stderr);
    assert_eq!(gen.stdout["verdict"], "partial", "{gen:#}");
    let failures: Vec<String> = gen.stdout["validation_failures"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    assert!(
        failures.iter().any(|f| f.contains("enum") && f.contains("accepts")),
        "the enum violation should name its path: {failures:?}"
    );
}

/// Fail-loud hooks (the default): a payload violating the contract exits
/// non-zero with the missing field named; valid payloads are mapped.
#[test]
fn fail_loud_hook_rejects_nonconforming_payload() {
    let temp = tempfile::tempdir().expect("tempdir");
    let schemas = flat_schemas();
    let diff = run_schema_diff(&schemas);
    let out_dir = temp.path().join("wrapper-loud");
    let gen = run_generator(
        &temp,
        "base.agent.adapter.loud",
        &[
            "--target-spec-json",
            &serde_json::to_string(
                &serde_json::json!({
                    "accepts": schemas["target_accepts"],
                    "returns": schemas["target_returns"]
                }),
            )
            .unwrap(),
            "--schema-diff-json",
            &serde_json::to_string(&diff).unwrap(),
            "--base-schema-json",
            &serde_json::to_string(
                &serde_json::json!({
                    "accepts": schemas["base_accepts"],
                    "returns": schemas["base_returns"]
                }),
            )
            .unwrap(),
            "--wrapper-mode",
            "script",
            "--base-script-path",
            write_base_entry(&temp, "base_entry_loud.py").as_str(),
            "--output-dir",
            out_dir.to_string_lossy().as_ref(),
        ],
    );
    assert!(gen.status.success(), "generator failed: {}", gen.stderr);

    let run_hook = |script: &Path, stdin: &str| {
        let mut child = Command::new("python3")
            .arg(script)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("hook should spawn");
        use std::io::Write;
        child
            .stdin
            .take()
            .expect("stdin available")
            .write_all(stdin.as_bytes())
            .expect("stdin write");
        child.wait_with_output().expect("hook should run")
    };

    let pre = out_dir.join("scripts/pre_map.py");
    // Conforming payload: renamed in place (task->query, topic->domain).
    let ok = run_hook(&pre, r#"{"task":"x","topic":"y"}"#);
    assert!(ok.status.success());
    let mapped: serde_json::Value = serde_json::from_slice(&ok.stdout).unwrap();
    assert_eq!(mapped, serde_json::json!({"query": "x", "domain": "y"}));

    // Missing a required mapped field: exit non-zero, field named on stderr.
    let missing = run_hook(&pre, r#"{"task":"x"}"#);
    assert!(
        !missing.status.success(),
        "missing required mapped field must fail loud"
    );
    let stderr = String::from_utf8_lossy(&missing.stderr);
    assert!(
        stderr.contains("topic"),
        "stderr should name the missing field, got: {stderr}"
    );

    // Unparseable payload: exit non-zero, no passthrough.
    let garbage = run_hook(&pre, "not json at all");
    assert!(
        !garbage.status.success(),
        "unparseable payload must fail loud, not pass through"
    );
}

/// `--fail-soft` restores the legacy passthrough: the un-mappable payload
/// flows through unchanged instead of failing the turn.
#[test]
fn fail_soft_opt_out_passes_through() {    let temp = tempfile::tempdir().expect("tempdir");
    let schemas = flat_schemas();
    let diff = run_schema_diff(&schemas);
    let out_dir = temp.path().join("wrapper-soft");
    let gen = run_generator(
        &temp,
        "base.agent.adapter.soft",
        &[
            "--target-spec-json",
            &serde_json::to_string(
                &serde_json::json!({
                    "accepts": schemas["target_accepts"],
                    "returns": schemas["target_returns"]
                }),
            )
            .unwrap(),
            "--schema-diff-json",
            &serde_json::to_string(&diff).unwrap(),
            "--base-schema-json",
            &serde_json::to_string(
                &serde_json::json!({
                    "accepts": schemas["base_accepts"],
                    "returns": schemas["base_returns"]
                }),
            )
            .unwrap(),
            "--wrapper-mode",
            "script",
            "--base-script-path",
            write_base_entry(&temp, "base_entry_soft.py").as_str(),
            "--fail-soft",
            "--output-dir",
            out_dir.to_string_lossy().as_ref(),
        ],
    );
    assert!(gen.status.success(), "generator failed: {}", gen.stderr);

    let mut child = Command::new("python3")
        .arg(out_dir.join("scripts/pre_map.py"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("hook should spawn");
    use std::io::Write;
    child
        .stdin
        .take()
        .expect("stdin available")
        .write_all(br#"{"nope": 1}"#)
        .expect("stdin write");
    let out = child.wait_with_output().expect("hook should run");
    assert!(out.status.success(), "fail-soft hook must not fail");
    let payload: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(
        payload,
        serde_json::json!({"nope": 1}),
        "fail-soft passthrough must leave the payload unchanged"
    );
}

/// Identical schemas need no mapper at all — and an unmapped passthrough is
/// genuinely correct there, so `ok` without hooks is the honest verdict.
#[test]
fn identical_schemas_need_no_hooks_and_report_ok() {
    let temp = tempfile::tempdir().expect("tempdir");
    let schema = string_object_schema(&["task"]);
    let diff = run_schema_diff(&serde_json::json!({
        "base_accepts": schema,
        "base_returns": schema,
        "target_accepts": schema,
        "target_returns": schema
    }));
    let gen = run_generator(
        &temp,
        "base.agent.adapter.same",
        &[
            "--target-spec-json",
            &serde_json::to_string(&serde_json::json!({ "accepts": schema })).unwrap(),
            "--schema-diff-json",
            &serde_json::to_string(&diff).unwrap(),
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
        "no mapper files for identical schemas: {files:?}"
    );
}

/// Fail-loud enforcement covers every schema-required field, not only the
/// renamed ones (#1255 review): when a type-invalid pair is dropped, the
/// dropped field is still required on the wire — a payload omitting it must
/// exit non-zero instead of reaching the base partially-conforming.
#[test]
fn fail_loud_requires_unrenamed_required_fields() {
    let temp = tempfile::tempdir().expect("tempdir");
    // target {task: str, tag: str} -> base {query: str, tag: int}: the
    // tag->tag rename is dropped (string vs integer), so `tag` stays
    // required on the wire but is NOT in the rename list.
    let schemas = serde_json::json!({
        "base_accepts": serde_json::json!({
            "type": "object",
            "required": ["query", "tag"],
            "properties": {
                "query": { "type": "string" },
                "tag": { "type": "integer" }
            }
        }),
        "base_returns": string_object_schema(&["summary"]),
        "target_accepts": serde_json::json!({
            "type": "object",
            "required": ["task", "tag"],
            "properties": {
                "task": { "type": "string" },
                "tag": { "type": "string" }
            }
        }),
        "target_returns": string_object_schema(&["result"])
    });
    let diff = run_schema_diff(&schemas);
    let out_dir = temp.path().join("wrapper-unrenamed");
    let gen = run_generator(
        &temp,
        "base.agent.adapter.unrenamed",
        &[
            "--target-spec-json",
            &serde_json::to_string(
                &serde_json::json!({
                    "accepts": schemas["target_accepts"],
                    "returns": schemas["target_returns"]
                }),
            )
            .unwrap(),
            "--schema-diff-json",
            &serde_json::to_string(&diff).unwrap(),
            "--base-schema-json",
            &serde_json::to_string(
                &serde_json::json!({
                    "accepts": schemas["base_accepts"],
                    "returns": schemas["base_returns"]
                }),
            )
            .unwrap(),
            "--wrapper-mode",
            "script",
            "--base-script-path",
            write_base_entry(&temp, "base_entry_unrenamed.py").as_str(),
            "--output-dir",
            out_dir.to_string_lossy().as_ref(),
        ],
    );
    assert!(gen.status.success(), "generator failed: {}", gen.stderr);
    let notes: Vec<String> = gen.stdout["notes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    assert!(
        notes.iter().any(|n| n.contains("dropped rename tag->tag")),
        "the tag rename should be dropped as type-invalid: {notes:?}"
    );

    let run_hook = |stdin: &str| {
        let mut child = Command::new("python3")
            .arg(out_dir.join("scripts/pre_map.py"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("hook should spawn");
        use std::io::Write;
        child
            .stdin
            .take()
            .expect("stdin available")
            .write_all(stdin.as_bytes())
            .expect("stdin write");
        child.wait_with_output().expect("hook should run")
    };

    // Missing the unrenamed required field: fail loud, field named.
    let missing = run_hook(r#"{"task":"x"}"#);
    assert!(
        !missing.status.success(),
        "omitting a required-but-unrenamed field must fail loud"
    );
    let stderr = String::from_utf8_lossy(&missing.stderr);
    assert!(
        stderr.contains("tag"),
        "stderr should name the unrenamed required field, got: {stderr}"
    );

    // Complete payload: renamed + passthrough fields both survive.
    let ok = run_hook(r#"{"task":"x","tag":"t"}"#);
    assert!(ok.status.success());
    let mapped: serde_json::Value = serde_json::from_slice(&ok.stdout).unwrap();
    assert_eq!(
        mapped,
        serde_json::json!({"query": "x", "tag": "t"}),
        "the dropped rename must leave the field passing through"
    );
}

/// Schema-derived notes are attacker-reachable (the adapter receives the
/// target spec from a requesting agent), so they must never land in a code
/// position of a generated hook (#1255 review): a note carrying a docstring
/// terminator plus Python must end up as an inert, newline-collapsed comment.
#[test]
fn hostile_notes_cannot_inject_code_into_generated_hooks() {
    let temp = tempfile::tempdir().expect("tempdir");
    let payload_notes = r#"evil: '"""' + "\n" + "print('PWNED')\nimport os\n""#;
    let diff = serde_json::json!({
        "accepts_compatible": false,
        "returns_compatible": false,
        "requires_input_mapping": true,
        "requires_output_mapping": false,
        "input_mappings": [{"from": "task", "to": "query"}],
        "output_mappings": [],
        "notes": [payload_notes]
    });
    let out_dir = temp.path().join("wrapper-evil");
    let gen = run_generator(
        &temp,
        "base.agent.adapter.evil",
        &[
            "--target-spec-json",
            r#"{"accepts":{"type":"object","required":["task"],"properties":{"task":{"type":"string"}}}}"#,
            "--schema-diff-json",
            &serde_json::to_string(&diff).unwrap(),
            "--wrapper-mode",
            "script",
            "--base-script-path",
            write_base_entry(&temp, "base_entry_evil.py").as_str(),
            "--output-dir",
            out_dir.to_string_lossy().as_ref(),
        ],
    );
    assert!(gen.status.success(), "generator failed: {}", gen.stderr);

    let pre = std::fs::read_to_string(out_dir.join("scripts/pre_map.py")).unwrap();
    // The safety property: every triple-quote sequence outside the two
    // docstring delimiters sits in a `#` comment — inert either way.
    let mut delimiters = 0;
    for line in pre.lines() {
        if line.contains("\"\"\"") {
            let trimmed = line.trim_start();
            // The opening delimiter has the docstring's first words on the
            // same line; the closing one stands alone.
            if trimmed.starts_with("\"\"\"") {
                delimiters += 1;
            } else {
                assert!(
                    trimmed.starts_with('#'),
                    "a triple-quote outside the docstring must be comment-prefixed:\n{line}"
                );
            }
        }
    }
    assert_eq!(
        delimiters, 2,
        "exactly the generated docstring terminators may close a string:\n{pre}"
    );
    assert!(
        pre.lines()
            .filter(|line| line.contains("import os") || line.contains("PWNED"))
            .all(|line| line.trim_start().starts_with('#')),
        "hostile statement text may only appear inside comments:\n{pre}"
    );
    assert!(
        pre.lines()
            .filter(|line| line.contains("PWNED"))
            .all(|line| line.trim_start().starts_with('#')),
        "every note line carrying hostile text must be comment-prefixed:\n{pre}"
    );

    // The hook must still run correctly — the note is inert text.
    let mut child = Command::new("python3")
        .arg(out_dir.join("scripts/pre_map.py"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("hook should spawn");
    use std::io::Write;
    child
        .stdin
        .take()
        .expect("stdin available")
        .write_all(br#"{"task":"clean"}"#)
        .expect("stdin write");
    let out = child.wait_with_output().expect("hook should run");
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        !stdout.contains("PWNED"),
        "injected code must not execute: {stdout}"
    );
    let mapped: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(mapped, serde_json::json!({"query": "clean"}));
}
