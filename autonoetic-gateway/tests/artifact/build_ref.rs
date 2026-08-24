//! Integration tests: `artifact_build` returns manifest/canonical digests and mints `artifact_ref` in `gateway.db`.

use std::sync::Arc;

use autonoetic_gateway::policy::PolicyEngine;
use autonoetic_gateway::runtime::content_store::ContentStore;
use autonoetic_gateway::runtime::tools::default_registry;
use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_types::agent::{AgentIdentity, AgentManifest};
use autonoetic_types::artifact::ArtifactRefScopeType;
use autonoetic_types::capability::Capability;
use autonoetic_types::config::GatewayConfig;
use tempfile::tempdir;
use crate::support::manifest_builder::TestManifest;

fn writer_manifest() -> AgentManifest {
    AgentManifest {
        agent: AgentIdentity {
            id: "coder.default".to_string(),
            name: "coder".to_string(),
            description: "test".to_string(),
            singleton: false,
            resident_idle_ttl_secs: None,
        },
        capabilities: vec![Capability::WriteAccess {
            scopes: vec!["*".to_string()],
        }],
        ..TestManifest::new().build()
    }
}

#[test]
fn test_artifact_build_mints_session_scoped_ref() -> anyhow::Result<()> {
    let temp = tempdir()?;
    let agents_dir = temp.path().join("agents");
    let gateway_dir = agents_dir.join(".gateway");
    std::fs::create_dir_all(&gateway_dir)?;

    let config = GatewayConfig {
        runtime_dir: gateway_dir.clone(),
        agents_dir: agents_dir.clone(),
        ..GatewayConfig::default()
    };

    let store = Arc::new(GatewayStore::open(&gateway_dir)?);
    let manifest = writer_manifest();
    let policy = PolicyEngine::new(manifest.clone());
    let registry = default_registry();

    let agent_dir = agents_dir.join("coder.default");
    std::fs::create_dir_all(&agent_dir)?;

    let cs = ContentStore::new(&gateway_dir)?;
    let h = cs.write(b"artifact body")?;
    cs.register_name("sess-a", "main.txt", &h)?;

    let args = serde_json::json!({ "inputs": ["main.txt"] });
    let out = registry.execute(
        "artifact_build",
        &manifest,
        &policy,
        &agent_dir,
        Some(&gateway_dir),
        &args.to_string(),
        Some("sess-a"),
        None,
        Some(&config),
        Some(store.clone()),
        None,
    )?;

    let v: serde_json::Value = serde_json::from_str(&out)?;
    assert_eq!(v.get("ok"), Some(&serde_json::json!(true)));
    let digest = v["artifact_manifest_digest"]
        .as_str()
        .expect("artifact_manifest_digest");
    let ar = v["artifact_ref"]
        .as_str()
        .expect("artifact_ref on first build");
    assert!(ar.starts_with("ar."));
    let scope = v["artifact_ref_scope"]
        .as_object()
        .expect("artifact_ref_scope object");
    assert_eq!(scope.get("type").and_then(|x| x.as_str()), Some("session"));
    assert_eq!(scope.get("id").and_then(|x| x.as_str()), Some("sess-a"));

    let resolved = store.resolve_artifact_ref(ArtifactRefScopeType::Session, "sess-a", ar)?;
    let rec = resolved.expect("ref resolves");
    assert_eq!(rec.artifact_manifest_digest, digest);
    Ok(())
}

#[test]
fn test_artifact_build_scopes_to_root_session_for_child_without_workflow() -> anyhow::Result<()> {
    let temp = tempdir()?;
    let agents_dir = temp.path().join("agents");
    let gateway_dir = agents_dir.join(".gateway");
    std::fs::create_dir_all(&gateway_dir)?;

    let config = GatewayConfig {
        runtime_dir: gateway_dir.clone(),
        agents_dir: agents_dir.clone(),
        ..GatewayConfig::default()
    };

    let store = Arc::new(GatewayStore::open(&gateway_dir)?);
    let manifest = writer_manifest();
    let policy = PolicyEngine::new(manifest.clone());
    let registry = default_registry();

    let agent_dir = agents_dir.join("coder.default");
    std::fs::create_dir_all(&agent_dir)?;

    let cs = ContentStore::new(&gateway_dir)?;
    let h = cs.write(b"artifact from child")?;
    cs.register_name("root-sess/coder.default-abc123", "main.py", &h)?;

    let args = serde_json::json!({ "inputs": ["main.py"] });
    let out = registry.execute(
        "artifact_build",
        &manifest,
        &policy,
        &agent_dir,
        Some(&gateway_dir),
        &args.to_string(),
        Some("root-sess/coder.default-abc123"),
        None,
        Some(&config),
        Some(store.clone()),
        None,
    )?;

    let v: serde_json::Value = serde_json::from_str(&out)?;
    assert_eq!(v.get("ok"), Some(&serde_json::json!(true)));
    let ar = v["artifact_ref"]
        .as_str()
        .expect("artifact_ref");
    let scope = v["artifact_ref_scope"]
        .as_object()
        .expect("artifact_ref_scope object");

    assert_eq!(scope.get("type").and_then(|x| x.as_str()), Some("session"));
    assert_eq!(
        scope.get("id").and_then(|x| x.as_str()),
        Some("root-sess"),
        "child session artifact must be scoped to root session, not the child session"
    );

    let resolved = store.resolve_artifact_ref(ArtifactRefScopeType::Session, "root-sess", ar)?;
    assert!(resolved.is_some(), "ref must resolve from root session scope");

    let resolved_child =
        store.resolve_artifact_ref(ArtifactRefScopeType::Session, "root-sess/coder.default-abc123", ar)?;
    assert!(
        resolved_child.is_none(),
        "ref must NOT be scoped to the child session"
    );

    let any_scope = store.resolve_artifact_ref_any_scope(ar, "root-sess/packager.default-def456")?;
    assert!(
        any_scope.is_some(),
        "sibling agent must resolve artifact via root session scope"
    );

    Ok(())
}

#[test]
fn test_artifact_build_mints_workflow_scoped_ref_when_indexed() -> anyhow::Result<()> {
    let temp = tempdir()?;
    let agents_dir = temp.path().join("agents");
    let gateway_dir = agents_dir.join(".gateway");
    std::fs::create_dir_all(&gateway_dir)?;

    let config = GatewayConfig {
        runtime_dir: gateway_dir.clone(),
        agents_dir: agents_dir.clone(),
        ..GatewayConfig::default()
    };

    let store = Arc::new(GatewayStore::open(&gateway_dir)?);
    store.set_workflow_index("demo-root", "wf-xyz")?;

    let manifest = writer_manifest();
    let policy = PolicyEngine::new(manifest.clone());
    let registry = default_registry();

    let agent_dir = agents_dir.join("coder.default");
    std::fs::create_dir_all(&agent_dir)?;

    let cs = ContentStore::new(&gateway_dir)?;
    let h = cs.write(b"x")?;
    cs.register_name("demo-root/child", "f.txt", &h)?;

    let args = serde_json::json!({ "inputs": ["f.txt"] });
    let out = registry.execute(
        "artifact_build",
        &manifest,
        &policy,
        &agent_dir,
        Some(&gateway_dir),
        &args.to_string(),
        Some("demo-root/child"),
        None,
        Some(&config),
        Some(store.clone()),
        None,
    )?;

    let v: serde_json::Value = serde_json::from_str(&out)?;
    let ar = v["artifact_ref"].as_str().expect("artifact_ref");
    let scope = v["artifact_ref_scope"]
        .as_object()
        .expect("artifact_ref_scope");
    assert_eq!(scope.get("type").and_then(|x| x.as_str()), Some("workflow"));
    assert_eq!(scope.get("id").and_then(|x| x.as_str()), Some("wf-xyz"));

    let resolved = store.resolve_artifact_ref(ArtifactRefScopeType::Workflow, "wf-xyz", ar)?;
    assert!(resolved.is_some());
    Ok(())
}

#[test]
fn test_artifact_build_reuse_does_not_mint_second_ref() -> anyhow::Result<()> {
    let temp = tempdir()?;
    let agents_dir = temp.path().join("agents");
    let gateway_dir = agents_dir.join(".gateway");
    std::fs::create_dir_all(&gateway_dir)?;

    let config = GatewayConfig {
        runtime_dir: gateway_dir.clone(),
        agents_dir: agents_dir.clone(),
        ..GatewayConfig::default()
    };

    let store = Arc::new(GatewayStore::open(&gateway_dir)?);
    let manifest = writer_manifest();
    let policy = PolicyEngine::new(manifest.clone());
    let registry = default_registry();

    let agent_dir = agents_dir.join("coder.default");
    std::fs::create_dir_all(&agent_dir)?;

    let cs = ContentStore::new(&gateway_dir)?;
    let h = cs.write(b"same")?;
    cs.register_name("s1", "a.txt", &h)?;

    let args = serde_json::json!({ "inputs": ["a.txt"] });
    let out1 = registry.execute(
        "artifact_build",
        &manifest,
        &policy,
        &agent_dir,
        Some(&gateway_dir),
        &args.to_string(),
        Some("s1"),
        None,
        Some(&config),
        Some(store.clone()),
        None,
    )?;
    let v1: serde_json::Value = serde_json::from_str(&out1)?;
    assert_eq!(v1.get("reused"), Some(&serde_json::json!(false)));
    assert!(v1.get("artifact_ref").is_some());

    let out2 = registry.execute(
        "artifact_build",
        &manifest,
        &policy,
        &agent_dir,
        Some(&gateway_dir),
        &args.to_string(),
        Some("s1"),
        None,
        Some(&config),
        Some(store.clone()),
        None,
    )?;
    let v2: serde_json::Value = serde_json::from_str(&out2)?;
    assert_eq!(v2.get("reused"), Some(&serde_json::json!(true)));
    assert_eq!(v1["artifact_ref"], v2["artifact_ref"]);
    assert_eq!(
        v1["artifact_canonical_digest"],
        v2["artifact_canonical_digest"]
    );
    Ok(())
}

fn reader_manifest() -> AgentManifest {
    AgentManifest {
        agent: AgentIdentity {
            id: "sealed_evaluator.default".to_string(),
            name: "sealed_evaluator".to_string(),
            description: "test".to_string(),
            singleton: false,
            resident_idle_ttl_secs: None,
        },
        capabilities: vec![Capability::ReadAccess {
            scopes: vec!["*".to_string()],
        }],
        ..TestManifest::new().build()
    }
}

// ── resolve (the universal front door, #312) ───────────────────────────────

/// Build an artifact, then `resolve` its `ar.*` ref (scope inferred from the
/// session) with include=files — the artifact path of the front door.
#[test]
fn test_resolve_artifact_metadata_and_files() -> anyhow::Result<()> {
    let temp = tempdir()?;
    let agents_dir = temp.path().join("agents");
    let gateway_dir = agents_dir.join(".gateway");
    std::fs::create_dir_all(&gateway_dir)?;
    let config = GatewayConfig {
        agents_dir: agents_dir.clone(),
        runtime_dir: gateway_dir.clone(),
        ..GatewayConfig::default()
    };
    let store = Arc::new(GatewayStore::open(&gateway_dir)?);
    let writer = writer_manifest();
    let writer_policy = PolicyEngine::new(writer.clone());
    let reader = reader_manifest();
    let reader_policy = PolicyEngine::new(reader.clone());
    let registry = default_registry();
    let agent_dir = agents_dir.join("coder.default");
    std::fs::create_dir_all(&agent_dir)?;

    let cs = ContentStore::new(&gateway_dir)?;
    let h = cs.write(b"resolved content")?;
    cs.register_name("sess-r", "data.py", &h)?;

    let build_out = registry.execute(
        "artifact_build", &writer, &writer_policy, &agent_dir, Some(&gateway_dir),
        &serde_json::json!({ "inputs": ["data.py"] }).to_string(),
        Some("sess-r"), None, Some(&config), Some(store.clone()), None,
    )?;
    let build_v: serde_json::Value = serde_json::from_str(&build_out)?;
    let artifact_ref = build_v["artifact_ref"].as_str().expect("artifact_ref");

    let out = registry.execute(
        "resolve", &reader, &reader_policy, &agent_dir, Some(&gateway_dir),
        &serde_json::json!({ "ref": artifact_ref, "include": "files" }).to_string(),
        Some("sess-r"), None, Some(&config), Some(store.clone()), None,
    )?;
    let v: serde_json::Value = serde_json::from_str(&out)?;
    assert_eq!(v["ok"], serde_json::json!(true));
    assert_eq!(v["kind"].as_str(), Some("artifact"));
    // Identity fields mirror artifact_inspect; raw art_* id is not surfaced.
    assert_eq!(v["artifact_ref"].as_str(), Some(artifact_ref));
    assert!(v["artifact_canonical_digest"].as_str().is_some());
    assert!(v["artifact_manifest_digest"].as_str().is_some());
    assert!(v.get("artifact_id").is_none(), "raw art_* id must not be exposed");
    assert_eq!(v["file_count"].as_u64(), Some(1));
    assert_eq!(v["files"][0]["name"].as_str(), Some("data.py"));
    // The file selector is the `file` param, not a packed `ar.<ref>:<file>`:
    // no per-file packed ref, and a top-level read_file shows the param form.
    assert!(
        v["files"][0].get("content_read_ref").is_none(),
        "packed ar.<ref>:<file> must not be surfaced"
    );
    assert_eq!(
        v["read_file"].as_str(),
        Some(format!("resolve(ref=\"{artifact_ref}\", include=\"content\", file=<name>)").as_str())
    );
    Ok(())
}

/// Read one file out of an artifact: address the artifact by its ref and name
/// the file with the separate `file` argument (no `ar.<ref>:<file>` packing).
#[test]
fn test_resolve_artifact_file_via_file_param() -> anyhow::Result<()> {
    let temp = tempdir()?;
    let agents_dir = temp.path().join("agents");
    let gateway_dir = agents_dir.join(".gateway");
    std::fs::create_dir_all(&gateway_dir)?;
    let config = GatewayConfig {
        agents_dir: agents_dir.clone(),
        runtime_dir: gateway_dir.clone(),
        ..GatewayConfig::default()
    };
    let store = Arc::new(GatewayStore::open(&gateway_dir)?);
    let writer = writer_manifest();
    let writer_policy = PolicyEngine::new(writer.clone());
    let reader = reader_manifest();
    let reader_policy = PolicyEngine::new(reader.clone());
    let registry = default_registry();
    let agent_dir = agents_dir.join("coder.default");
    std::fs::create_dir_all(&agent_dir)?;

    let cs = ContentStore::new(&gateway_dir)?;
    let h = cs.write(b"resolved content")?;
    cs.register_name("sess-r", "data.py", &h)?;

    let build_out = registry.execute(
        "artifact_build", &writer, &writer_policy, &agent_dir, Some(&gateway_dir),
        &serde_json::json!({ "inputs": ["data.py"] }).to_string(),
        Some("sess-r"), None, Some(&config), Some(store.clone()), None,
    )?;
    let build_v: serde_json::Value = serde_json::from_str(&build_out)?;
    let artifact_ref = build_v["artifact_ref"].as_str().expect("artifact_ref").to_string();

    // Param form succeeds and returns the file bytes.
    let out = registry.execute(
        "resolve", &reader, &reader_policy, &agent_dir, Some(&gateway_dir),
        &serde_json::json!({ "ref": artifact_ref, "include": "content", "file": "data.py" }).to_string(),
        Some("sess-r"), None, Some(&config), Some(store.clone()), None,
    )?;
    let v: serde_json::Value = serde_json::from_str(&out)?;
    assert_eq!(v["ok"], serde_json::json!(true));
    assert_eq!(v["kind"].as_str(), Some("artifact_file"));
    assert_eq!(v["file"].as_str(), Some("data.py"));
    assert_eq!(v["content"].as_str(), Some("resolved content"));

    // include=content without a file is a validation error pointing at `file`.
    let out = registry.execute(
        "resolve", &reader, &reader_policy, &agent_dir, Some(&gateway_dir),
        &serde_json::json!({ "ref": artifact_ref, "include": "content" }).to_string(),
        Some("sess-r"), None, Some(&config), Some(store.clone()), None,
    )?;
    let v: serde_json::Value = serde_json::from_str(&out)?;
    assert_eq!(v["ok"], serde_json::json!(false));
    assert_eq!(v["error_type"].as_str(), Some("validation"));

    // The legacy packed `ar.<ref>:<file>` form is rejected with a nudge to `file`.
    let packed = format!("{artifact_ref}:data.py");
    let out = registry.execute(
        "resolve", &reader, &reader_policy, &agent_dir, Some(&gateway_dir),
        &serde_json::json!({ "ref": packed, "include": "content" }).to_string(),
        Some("sess-r"), None, Some(&config), Some(store.clone()), None,
    )?;
    let v: serde_json::Value = serde_json::from_str(&out)?;
    assert_eq!(v["ok"], serde_json::json!(false));
    assert_eq!(v["error_type"].as_str(), Some("validation"));
    assert!(
        v["message"].as_str().unwrap_or_default().contains("file"),
        "nudge must point at the `file` parameter, got: {}", v["message"]
    );
    Ok(())
}

/// A missing artifact ref resolves to a structured not-found, not a panic.
#[test]
fn test_resolve_artifact_missing_returns_not_found() -> anyhow::Result<()> {
    let temp = tempdir()?;
    let agents_dir = temp.path().join("agents");
    let gateway_dir = agents_dir.join(".gateway");
    std::fs::create_dir_all(&gateway_dir)?;
    let config = GatewayConfig {
        agents_dir: agents_dir.clone(),
        runtime_dir: gateway_dir.clone(),
        ..GatewayConfig::default()
    };
    let store = Arc::new(GatewayStore::open(&gateway_dir)?);
    let reader = reader_manifest();
    let reader_policy = PolicyEngine::new(reader.clone());
    let registry = default_registry();
    let agent_dir = agents_dir.join("coder.default");
    std::fs::create_dir_all(&agent_dir)?;

    let out = registry.execute(
        "resolve", &reader, &reader_policy, &agent_dir, Some(&gateway_dir),
        &serde_json::json!({ "ref": "ar.deadbeef0000" }).to_string(),
        Some("sess-r"), None, Some(&config), Some(store.clone()), None,
    )?;
    let v: serde_json::Value = serde_json::from_str(&out)?;
    assert_eq!(v["ok"], serde_json::json!(false));
    Ok(())
}

/// Resolve a content handle: metadata reports existence; include=content
/// returns the bytes.
#[test]
fn test_resolve_content_metadata_and_content() -> anyhow::Result<()> {
    let temp = tempdir()?;
    let agents_dir = temp.path().join("agents");
    let gateway_dir = agents_dir.join(".gateway");
    std::fs::create_dir_all(&gateway_dir)?;
    let config = GatewayConfig {
        agents_dir: agents_dir.clone(),
        runtime_dir: gateway_dir.clone(),
        ..GatewayConfig::default()
    };
    let store = Arc::new(GatewayStore::open(&gateway_dir)?);
    let reader = reader_manifest();
    let reader_policy = PolicyEngine::new(reader.clone());
    let registry = default_registry();
    let agent_dir = agents_dir.join("coder.default");
    std::fs::create_dir_all(&agent_dir)?;

    let cs = ContentStore::new(&gateway_dir)?;
    let h = cs.write(b"hello resolve")?;
    cs.register_name("sess-c", "note.txt", &h)?;

    let meta = registry.execute(
        "resolve", &reader, &reader_policy, &agent_dir, Some(&gateway_dir),
        &serde_json::json!({ "ref": "note.txt" }).to_string(),
        Some("sess-c"), None, Some(&config), Some(store.clone()), None,
    )?;
    let mv: serde_json::Value = serde_json::from_str(&meta)?;
    assert_eq!(mv["ok"], serde_json::json!(true));
    assert_eq!(mv["kind"].as_str(), Some("content"));
    assert_eq!(mv["exists"], serde_json::json!(true));

    let body = registry.execute(
        "resolve", &reader, &reader_policy, &agent_dir, Some(&gateway_dir),
        &serde_json::json!({ "ref": "note.txt", "include": "content" }).to_string(),
        Some("sess-c"), None, Some(&config), Some(store.clone()), None,
    )?;
    let bv: serde_json::Value = serde_json::from_str(&body)?;
    assert_eq!(bv["content"].as_str(), Some("hello resolve"));
    Ok(())
}
