mod support;

use autonoetic_gateway::policy::PolicyEngine;
use autonoetic_gateway::runtime::content_store::ContentStore;
use autonoetic_gateway::runtime::tools::{default_registry, NativeTool};
use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_types::agent::{AgentIdentity, AgentManifest, RuntimeDeclaration};
use autonoetic_types::artifact::ArtifactKind;
use autonoetic_types::capability::Capability;
use autonoetic_types::config::GatewayConfig;
use std::path::Path;
use std::sync::Arc;
use tempfile::tempdir;

fn zero_cap_skill_md(agent_id: &str) -> String {
    format!(
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
  description: "Test agent for inspect"
execution_mode: script
script_entry: main.py
---
# Test agent
"#
    )
}

fn manifest_with_read_access(agent_id: &str) -> AgentManifest {
    AgentManifest {
        version: "1.0".to_string(),
        runtime: RuntimeDeclaration {
            engine: "autonoetic".to_string(),
            gateway_version: "0.1.0".to_string(),
            sdk_version: "0.1.0".to_string(),
            runtime_type: "stateful".to_string(),
            sandbox: "bubblewrap".to_string(),
            runtime_lock: "runtime.lock".to_string(),
        },
        agent: AgentIdentity {
            id: agent_id.to_string(),
            name: agent_id.to_string(),
            description: "Test".to_string(),
        },
        capabilities: vec![
            Capability::ReadAccess {
                scopes: vec!["self.*".to_string(), "skills/*".to_string()],
            },
            Capability::AgentRevision {
                patterns: vec!["*".to_string()],
            },
        ],
        llm_preset: None,
        llm_config: None,
        limits: None,
        background: None,
        disclosure: None,
        io: None,
        middleware: None,
        execution_mode: Default::default(),
        script_entry: None,
        script_input_mode: Default::default(),
        gateway_url: None,
        gateway_token: None,
        allowed_tool_tiers: vec![],
        agentskills_import: None,
        compression: None,
        sandbox_network: autonoetic_types::agent::SandboxNetworkPolicy::default(),
    }
}

fn build_and_promote_agent(
    base_dir: &Path,
    agent_id: &str,
) -> (String, std::path::PathBuf, Arc<GatewayStore>) {
    let gateway_dir = base_dir.join(".gateway");
    std::fs::create_dir_all(&gateway_dir).unwrap();
    let content_store = ContentStore::new(&gateway_dir).unwrap();
    let artifact_store =
        autonoetic_gateway::artifact_store::ArtifactStore::new(&gateway_dir).unwrap();
    let session_id = "session-builder";

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

    let skill_md = zero_cap_skill_md(agent_id);
    let main_py = "#!/usr/bin/env python3\nimport json\nprint(json.dumps({'status': 'ok'}))\n";

    for (path, content) in [
        ("SKILL.md", skill_md.as_bytes()),
        ("runtime.lock", runtime_lock.as_bytes()),
        ("main.py", main_py.as_bytes()),
    ] {
        let handle = content_store.write(content).unwrap();
        content_store
            .register_name(session_id, path, &handle)
            .unwrap();
    }

    let bundle = artifact_store
        .build_with_kind(
            &[
                "SKILL.md".to_string(),
                "runtime.lock".to_string(),
                "main.py".to_string(),
            ],
            Some(&["main.py".to_string()]),
            None,
            ArtifactKind::AgentBundle,
            session_id,
        )
        .unwrap();

    let agents_dir = base_dir.join("agents");
    let builder_dir = agents_dir.join("planner.default");
    std::fs::create_dir_all(&builder_dir).unwrap();

    let config = GatewayConfig {
        agents_dir,
        ..Default::default()
    };
    let store = Arc::new(GatewayStore::open(&gateway_dir).unwrap());
    let registry = default_registry();
    let builder = manifest_with_read_access("planner.default");
    let policy = PolicyEngine::new(builder.clone());

    let rev_args = serde_json::json!({
        "agent_id": agent_id,
        "artifact_id": bundle.artifact_id,
    });
    let rev_result = registry
        .execute(
            "agent_revision_create",
            &builder,
            &policy,
            &builder_dir,
            Some(&gateway_dir),
            &serde_json::to_string(&rev_args).unwrap(),
            Some(session_id),
            None,
            Some(&config),
            Some(store.clone()),
            None,
        )
        .expect("revision create should succeed");
    let rev_parsed: serde_json::Value = serde_json::from_str(&rev_result).unwrap();
    let revision_id = rev_parsed
        .get("revision_id")
        .and_then(|v| v.as_str())
        .unwrap()
        .to_string();

    let promote_args = serde_json::json!({
        "agent_id": agent_id,
        "revision_id": revision_id,
        "reason": "integration test",
    });
    let promote_result = registry
        .execute(
            "agent_revision_promote",
            &builder,
            &policy,
            &builder_dir,
            Some(&gateway_dir),
            &serde_json::to_string(&promote_args).unwrap(),
            Some(session_id),
            None,
            Some(&config),
            Some(store.clone()),
            None,
        )
        .expect("promote should succeed");
    let promote_parsed: serde_json::Value = serde_json::from_str(&promote_result).unwrap();
    assert_eq!(
        promote_parsed.get("ok").and_then(|v| v.as_bool()),
        Some(true),
        "promote should succeed: {promote_result}"
    );

    (bundle.artifact_id, gateway_dir, store)
}

#[test]
fn metadata_only_without_source() {
    let temp = tempdir().unwrap();
    let agent_id = "test.inspect-meta";
    let (artifact_id, gateway_dir, store) = build_and_promote_agent(temp.path(), agent_id);

    let agents_dir = temp.path().join("agents");
    let caller_dir = agents_dir.join("caller");
    std::fs::create_dir_all(&caller_dir).unwrap();
    let config = GatewayConfig {
        agents_dir,
        ..Default::default()
    };

    let registry = default_registry();
    let caller = manifest_with_read_access("caller");
    let policy = PolicyEngine::new(caller.clone());

    let args = serde_json::json!({
        "agent_id": agent_id,
    });
    let result = registry
        .execute(
            "agent_inspect",
            &caller,
            &policy,
            &caller_dir,
            Some(&gateway_dir),
            &serde_json::to_string(&args).unwrap(),
            Some("session-caller"),
            None,
            Some(&config),
            Some(store),
            None,
        )
        .expect("agent_inspect should succeed");

    let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
    assert_eq!(parsed.get("ok").and_then(|v| v.as_bool()), Some(true));
    assert_eq!(parsed["agent_id"].as_str(), Some(agent_id));
    assert!(parsed["alias"]["revision_id"].is_string());
    assert!(parsed["alias"]["short_ref"].is_string());
    assert!(parsed["revision"]["status"].is_string());
    assert_eq!(parsed["revision"]["trust_domain"].as_str(), Some("local"));
    assert_eq!(parsed["revision"]["artifact_id"].as_str(), Some(artifact_id.as_str()));
    assert!(parsed["skill"]["agent"]["id"].is_string());
    assert!(parsed["files"].as_array().unwrap().len() >= 3);
    assert!(parsed.get("source").is_none(), "source should not be present when include_source is false");
}

#[test]
fn includes_source_when_requested_for_local_agent() {
    let temp = tempdir().unwrap();
    let agent_id = "test.inspect-source";
    let (_artifact_id, gateway_dir, store) = build_and_promote_agent(temp.path(), agent_id);

    let agents_dir = temp.path().join("agents");
    let caller_dir = agents_dir.join("caller");
    std::fs::create_dir_all(&caller_dir).unwrap();
    let config = GatewayConfig {
        agents_dir,
        ..Default::default()
    };

    let registry = default_registry();
    let caller = manifest_with_read_access("caller");
    let policy = PolicyEngine::new(caller.clone());

    let args = serde_json::json!({
        "agent_id": agent_id,
        "include_source": true,
    });
    let result = registry
        .execute(
            "agent_inspect",
            &caller,
            &policy,
            &caller_dir,
            Some(&gateway_dir),
            &serde_json::to_string(&args).unwrap(),
            Some("session-caller"),
            None,
            Some(&config),
            Some(store),
            None,
        )
        .expect("agent_inspect should succeed");

    let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
    assert_eq!(parsed.get("ok").and_then(|v| v.as_bool()), Some(true));

    let source = parsed.get("source").expect("source should be present");
    let source_obj = source.as_object().expect("source should be an object");
    assert!(source_obj.contains_key("SKILL.md"), "source should contain SKILL.md");
    assert!(source_obj.contains_key("main.py"), "source should contain main.py");
    assert!(source_obj.contains_key("runtime.lock"), "source should contain runtime.lock");

    let main_py = source_obj["main.py"].as_str().unwrap();
    assert!(main_py.contains("import json"), "main.py should contain source code");
}

#[test]
fn returns_error_for_unknown_agent() {
    let temp = tempdir().unwrap();
    let gateway_dir = temp.path().join(".gateway");
    std::fs::create_dir_all(&gateway_dir).unwrap();
    let store = Arc::new(GatewayStore::open(&gateway_dir).unwrap());

    let agents_dir = temp.path().join("agents");
    let caller_dir = agents_dir.join("caller");
    std::fs::create_dir_all(&caller_dir).unwrap();
    let config = GatewayConfig {
        agents_dir,
        ..Default::default()
    };

    let registry = default_registry();
    let caller = manifest_with_read_access("caller");
    let policy = PolicyEngine::new(caller.clone());

    let args = serde_json::json!({
        "agent_id": "nonexistent.agent",
    });
    let result = registry.execute(
        "agent_inspect",
        &caller,
        &policy,
        &caller_dir,
        Some(&gateway_dir),
        &serde_json::to_string(&args).unwrap(),
        Some("session-caller"),
        None,
        Some(&config),
        Some(store),
        None,
    );

    assert!(result.is_err(), "should fail for unknown agent");
    let err = result.unwrap_err().to_string();
    assert!(err.contains("not installed"), "error should mention not installed: {err}");
}

#[test]
fn tool_requires_read_access_capability() {
    let manifest_no_read = AgentManifest {
        version: "1.0".to_string(),
        runtime: RuntimeDeclaration {
            engine: "autonoetic".to_string(),
            gateway_version: "0.1.0".to_string(),
            sdk_version: "0.1.0".to_string(),
            runtime_type: "stateful".to_string(),
            sandbox: "bubblewrap".to_string(),
            runtime_lock: "runtime.lock".to_string(),
        },
        agent: AgentIdentity {
            id: "no-read-agent".to_string(),
            name: "no-read-agent".to_string(),
            description: "Test".to_string(),
        },
        capabilities: vec![],
        llm_preset: None,
        llm_config: None,
        limits: None,
        background: None,
        disclosure: None,
        io: None,
        middleware: None,
        execution_mode: Default::default(),
        script_entry: None,
        script_input_mode: Default::default(),
        gateway_url: None,
        gateway_token: None,
        allowed_tool_tiers: vec![],
        agentskills_import: None,
        compression: None,
        sandbox_network: autonoetic_types::agent::SandboxNetworkPolicy::default(),
    };

    let tool = autonoetic_gateway::runtime::tools::agent_inspect::AgentInspectTool;
    assert!(!tool.is_available(&manifest_no_read), "should not be available without ReadAccess");
}

/// Helper: invoke `agent_inspect` with `include_source: true` and return the
/// parsed result for an agent that was previously promoted via
/// `build_and_promote_agent`.
fn inspect_with_source(
    gateway_dir: &Path,
    agents_dir: &Path,
    store: Arc<GatewayStore>,
    agent_id: &str,
) -> serde_json::Value {
    let caller_dir = agents_dir.join("caller");
    std::fs::create_dir_all(&caller_dir).unwrap();
    let config = GatewayConfig {
        agents_dir: agents_dir.to_path_buf(),
        ..Default::default()
    };
    let registry = default_registry();
    let caller = manifest_with_read_access("caller");
    let policy = PolicyEngine::new(caller.clone());
    let args = serde_json::json!({
        "agent_id": agent_id,
        "include_source": true,
    });
    let result = registry
        .execute(
            "agent_inspect",
            &caller,
            &policy,
            &caller_dir,
            Some(gateway_dir),
            &serde_json::to_string(&args).unwrap(),
            Some("session-caller"),
            None,
            Some(&config),
            Some(store),
            None,
        )
        .expect("agent_inspect should succeed");
    serde_json::from_str(&result).unwrap()
}

/// Locate the on-disk revision directory for a promoted agent so a test can
/// drop additional fixture files (junk we want filtered, oversized files,
/// binaries, etc.) into it after promotion.
fn revision_dir_for(
    gateway_dir: &Path,
    store: &GatewayStore,
    agent_id: &str,
) -> std::path::PathBuf {
    let alias = store.resolve_alias(agent_id).unwrap().unwrap();
    gateway_dir
        .join("revisions")
        .join("agents")
        .join(agent_id)
        .join(&alias.revision_id)
}

#[test]
fn excludes_pycache_venv_and_socket_files() {
    let temp = tempdir().unwrap();
    let agent_id = "test.inspect-junk";
    let (_artifact_id, gateway_dir, store) = build_and_promote_agent(temp.path(), agent_id);

    // Drop in the kinds of artifacts that bloated the real session: a
    // __pycache__/ subdir with .pyc files, a venv/ subdir, a stale .sock, and
    // a binary .so. None of these should appear in `source` OR in `files`.
    let rev_dir = revision_dir_for(&gateway_dir, &store, agent_id);
    std::fs::create_dir_all(rev_dir.join("__pycache__")).unwrap();
    std::fs::write(
        rev_dir.join("__pycache__").join("foo.cpython-312.pyc"),
        b"\x00\x00\x00bytecode garbage",
    )
    .unwrap();
    std::fs::create_dir_all(rev_dir.join("venv").join("lib")).unwrap();
    std::fs::write(rev_dir.join("venv").join("lib").join("site.py"), b"# venv")
        .unwrap();
    std::fs::write(rev_dir.join("autonoetic-abc.sock"), b"").unwrap();
    std::fs::write(rev_dir.join("native.so"), b"\x7fELFbinary").unwrap();

    let parsed = inspect_with_source(&gateway_dir, &temp.path().join("agents"), store, agent_id);
    let source = parsed["source"]
        .as_object()
        .expect("source should be present");

    // Real source must still be there.
    assert!(source.contains_key("SKILL.md"));
    assert!(source.contains_key("main.py"));

    // None of the excluded artifacts should appear in source.
    for key in source.keys() {
        assert!(
            !key.starts_with("__pycache__"),
            "source must not include __pycache__ files, found {key}"
        );
        assert!(
            !key.starts_with("venv"),
            "source must not include venv files, found {key}"
        );
        assert!(
            !key.ends_with(".sock"),
            "source must not include socket files, found {key}"
        );
        assert!(
            !key.ends_with(".so"),
            "source must not include shared libraries, found {key}"
        );
    }

    // The `files` list also drops them — they're useless noise even as names.
    let files = parsed["files"].as_array().expect("files list");
    for f in files {
        let s = f.as_str().unwrap();
        assert!(!s.starts_with("__pycache__"), "files list leaked: {s}");
        assert!(!s.starts_with("venv"), "files list leaked: {s}");
        assert!(!s.ends_with(".sock"), "files list leaked: {s}");
        assert!(!s.ends_with(".so"), "files list leaked: {s}");
    }

    // Diagnostic surfaces: the caller should be able to see WHAT was dropped.
    let excluded_dirs: Vec<&str> = parsed["excluded_directories"]
        .as_array()
        .expect("excluded_directories should be present")
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert!(excluded_dirs.contains(&"__pycache__"));
    assert!(excluded_dirs.contains(&"venv"));

    let excluded_file_reasons: Vec<(&str, &str)> = parsed["excluded_files"]
        .as_array()
        .expect("excluded_files should be present")
        .iter()
        .map(|v| {
            (
                v["path"].as_str().unwrap(),
                v["reason"].as_str().unwrap(),
            )
        })
        .collect();
    assert!(
        excluded_file_reasons
            .iter()
            .any(|(p, r)| *p == "autonoetic-abc.sock" && *r == "excluded_suffix"),
        "sock should be reported as excluded_suffix, got: {excluded_file_reasons:?}"
    );
    assert!(
        excluded_file_reasons
            .iter()
            .any(|(p, r)| *p == "native.so" && r.starts_with("excluded_extension")),
        "native.so should be reported as excluded_extension, got: {excluded_file_reasons:?}"
    );
}

#[test]
fn truncates_oversized_text_file_and_reports_original_size() {
    let temp = tempdir().unwrap();
    let agent_id = "test.inspect-big";
    let (_artifact_id, gateway_dir, store) = build_and_promote_agent(temp.path(), agent_id);

    // Drop a 200 KiB plain-text file into the revision dir. The per-file cap
    // (64 KiB) should truncate it; the truncation must be reported with the
    // original byte size so the LLM knows it didn't see the whole thing.
    let rev_dir = revision_dir_for(&gateway_dir, &store, agent_id);
    let oversize_bytes = 200 * 1024;
    let large = "abcdefghijklmnop".repeat(oversize_bytes / 16);
    assert_eq!(large.len(), oversize_bytes);
    std::fs::write(rev_dir.join("big.txt"), &large).unwrap();

    let parsed = inspect_with_source(&gateway_dir, &temp.path().join("agents"), store, agent_id);

    let source = parsed["source"].as_object().expect("source");
    let big = source["big.txt"].as_str().expect("big.txt should be present");
    assert!(
        big.len() <= 64 * 1024,
        "big.txt should be truncated to <= 64 KiB, got {}",
        big.len()
    );

    let truncated = parsed["truncated_files"]
        .as_array()
        .expect("truncated_files should be present");
    let entry = truncated
        .iter()
        .find(|v| v["path"].as_str() == Some("big.txt"))
        .expect("big.txt should appear in truncated_files");
    assert_eq!(
        entry["original_bytes"].as_u64(),
        Some(oversize_bytes as u64)
    );
    assert_eq!(entry["included_bytes"].as_u64(), Some(64 * 1024));
}

#[test]
fn skips_binary_content_files() {
    let temp = tempdir().unwrap();
    let agent_id = "test.inspect-binary";
    let (_artifact_id, gateway_dir, store) = build_and_promote_agent(temp.path(), agent_id);

    // A file with no excluded extension (so it makes it past the extension
    // filter) but with NUL bytes in its content should be skipped from
    // `source` via the binary heuristic.
    let rev_dir = revision_dir_for(&gateway_dir, &store, agent_id);
    let mut blob = Vec::with_capacity(1024);
    blob.extend_from_slice(b"prefix");
    blob.push(0);
    blob.extend_from_slice(b"middle");
    blob.push(0);
    blob.extend_from_slice(b"suffix");
    std::fs::write(rev_dir.join("opaque.data"), &blob).unwrap();

    let parsed = inspect_with_source(&gateway_dir, &temp.path().join("agents"), store, agent_id);

    let source = parsed["source"].as_object().expect("source");
    assert!(
        !source.contains_key("opaque.data"),
        "binary file must not be in source map"
    );

    let skipped = parsed["skipped_files"]
        .as_array()
        .expect("skipped_files should be present");
    assert!(
        skipped
            .iter()
            .any(|v| v["path"].as_str() == Some("opaque.data")
                && v["reason"].as_str() == Some("binary_content")),
        "opaque.data should be reported as binary_content, got: {skipped:?}"
    );
}

#[test]
fn total_source_size_cap_drops_remaining_files() {
    let temp = tempdir().unwrap();
    let agent_id = "test.inspect-totalcap";
    let (_artifact_id, gateway_dir, store) = build_and_promote_agent(temp.path(), agent_id);

    // Add several files near the per-file cap so their combined size pushes
    // past the 256 KiB total cap. The last file added should be dropped with
    // reason="total_size_cap" rather than mid-file truncated.
    let rev_dir = revision_dir_for(&gateway_dir, &store, agent_id);
    let chunk = "x".repeat(60 * 1024);
    for i in 0..6 {
        // Sorted lex: file_a then file_b ... so file_f is processed last.
        let name = format!("z_big_{}.txt", (b'a' + i) as char);
        std::fs::write(rev_dir.join(&name), &chunk).unwrap();
    }

    let parsed = inspect_with_source(&gateway_dir, &temp.path().join("agents"), store, agent_id);

    let source = parsed["source"].as_object().expect("source");
    let total: usize = source.values().map(|v| v.as_str().unwrap().len()).sum();
    assert!(
        total <= 256 * 1024,
        "total source bytes should respect the cap, got {total}"
    );

    let skipped = parsed["skipped_files"]
        .as_array()
        .expect("skipped_files should be present");
    assert!(
        skipped
            .iter()
            .any(|v| v["reason"].as_str() == Some("total_size_cap")),
        "at least one file should be dropped with reason=total_size_cap, got: {skipped:?}"
    );

    // The reported aggregate matches what we actually returned.
    assert_eq!(
        parsed["source_total_bytes"].as_u64().unwrap() as usize,
        total
    );
}
