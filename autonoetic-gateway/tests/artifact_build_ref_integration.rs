//! Integration tests: `artifact.build` returns `artifact_digest` and mints `artifact_ref` in `gateway.db`.

use std::sync::Arc;

use autonoetic_gateway::policy::PolicyEngine;
use autonoetic_gateway::runtime::content_store::ContentStore;
use autonoetic_gateway::runtime::tools::default_registry;
use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_types::agent::{AgentIdentity, AgentManifest, RuntimeDeclaration};
use autonoetic_types::artifact::ArtifactRefScopeType;
use autonoetic_types::capability::Capability;
use autonoetic_types::config::GatewayConfig;
use tempfile::tempdir;

fn writer_manifest() -> AgentManifest {
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
            id: "coder.default".to_string(),
            name: "coder".to_string(),
            description: "test".to_string(),
        },
        capabilities: vec![Capability::WriteAccess {
            scopes: vec!["*".to_string()],
        }],
        llm_config: None,
        limits: None,
        background: None,
        disclosure: None,
        io: None,
        middleware: None,
        execution_mode: Default::default(),
        script_entry: None,
        gateway_url: None,
        gateway_token: None,

        response_contract: None,
    }
}

#[test]
fn test_artifact_build_mints_session_scoped_ref() -> anyhow::Result<()> {
    let temp = tempdir()?;
    let agents_dir = temp.path().join("agents");
    let gateway_dir = agents_dir.join(".gateway");
    std::fs::create_dir_all(&gateway_dir)?;

    let config = GatewayConfig {
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
        "artifact.build",
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
    let digest = v["digest"].as_str().expect("digest");
    assert_eq!(v["artifact_digest"].as_str(), Some(digest));
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
    assert_eq!(rec.artifact_digest, digest);
    Ok(())
}

#[test]
fn test_artifact_build_mints_workflow_scoped_ref_when_indexed() -> anyhow::Result<()> {
    let temp = tempdir()?;
    let agents_dir = temp.path().join("agents");
    let gateway_dir = agents_dir.join(".gateway");
    std::fs::create_dir_all(&gateway_dir)?;

    let config = GatewayConfig {
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
        "artifact.build",
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
        "artifact.build",
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
        "artifact.build",
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
    assert!(v2.get("artifact_ref").is_none());
    assert_eq!(v1["artifact_id"], v2["artifact_id"]);
    Ok(())
}

fn reader_manifest() -> AgentManifest {
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
            id: "evaluator.default".to_string(),
            name: "evaluator".to_string(),
            description: "test".to_string(),
        },
        capabilities: vec![Capability::ReadAccess {
            scopes: vec!["*".to_string()],
        }],
        llm_config: None,
        limits: None,
        background: None,
        disclosure: None,
        io: None,
        middleware: None,
        execution_mode: Default::default(),
        script_entry: None,
        gateway_url: None,
        gateway_token: None,

        response_contract: None,
    }
}

#[test]
fn test_artifact_resolve_ref_success() -> anyhow::Result<()> {
    let temp = tempdir()?;
    let agents_dir = temp.path().join("agents");
    let gateway_dir = agents_dir.join(".gateway");
    std::fs::create_dir_all(&gateway_dir)?;

    let config = GatewayConfig {
        agents_dir: agents_dir.clone(),
        ..GatewayConfig::default()
    };

    let store = Arc::new(GatewayStore::open(&gateway_dir)?);

    let writer = writer_manifest();
    let writer_policy = PolicyEngine::new(writer.clone());
    let registry = default_registry();

    let agent_dir = agents_dir.join("coder.default");
    std::fs::create_dir_all(&agent_dir)?;

    let cs = ContentStore::new(&gateway_dir)?;
    let h = cs.write(b"resolved content")?;
    cs.register_name("sess-resolve", "data.py", &h)?;

    let build_args = serde_json::json!({ "inputs": ["data.py"] });
    let build_out = registry.execute(
        "artifact.build",
        &writer,
        &writer_policy,
        &agent_dir,
        Some(&gateway_dir),
        &build_args.to_string(),
        Some("sess-resolve"),
        None,
        Some(&config),
        Some(store.clone()),
        None,
    )?;
    let build_v: serde_json::Value = serde_json::from_str(&build_out)?;
    let artifact_ref = build_v["artifact_ref"].as_str().expect("artifact_ref");
    let artifact_digest = build_v["digest"].as_str().expect("digest");
    let artifact_id = build_v["artifact_id"].as_str().expect("artifact_id");

    let reader = reader_manifest();
    let reader_policy = PolicyEngine::new(reader.clone());

    let resolve_args = serde_json::json!({
        "ref_id": artifact_ref,
        "scope_type": "session",
        "scope_id": "sess-resolve"
    });
    let resolve_out = registry.execute(
        "artifact.resolve_ref",
        &reader,
        &reader_policy,
        &agent_dir,
        Some(&gateway_dir),
        &resolve_args.to_string(),
        Some("sess-resolve"),
        None,
        Some(&config),
        Some(store.clone()),
        None,
    )?;

    let resolve_v: serde_json::Value = serde_json::from_str(&resolve_out)?;
    assert_eq!(resolve_v.get("ok"), Some(&serde_json::json!(true)));
    assert_eq!(resolve_v["artifact_id"].as_str(), Some(artifact_id));
    assert_eq!(resolve_v["artifact_digest"].as_str(), Some(artifact_digest));
    assert!(resolve_v["files"].as_array().unwrap().len() == 1);
    assert_eq!(resolve_v["files"][0]["name"].as_str(), Some("data.py"));
    assert!(resolve_v["ref_created_at"].as_str().is_some());
    assert_eq!(resolve_v["ref_created_by"].as_str(), Some("coder.default"));
    Ok(())
}

#[test]
fn test_artifact_resolve_ref_wrong_scope_fails() -> anyhow::Result<()> {
    let temp = tempdir()?;
    let agents_dir = temp.path().join("agents");
    let gateway_dir = agents_dir.join(".gateway");
    std::fs::create_dir_all(&gateway_dir)?;

    let config = GatewayConfig {
        agents_dir: agents_dir.clone(),
        ..GatewayConfig::default()
    };

    let store = Arc::new(GatewayStore::open(&gateway_dir)?);

    let writer = writer_manifest();
    let writer_policy = PolicyEngine::new(writer.clone());
    let registry = default_registry();

    let agent_dir = agents_dir.join("coder.default");
    std::fs::create_dir_all(&agent_dir)?;

    let cs = ContentStore::new(&gateway_dir)?;
    let h = cs.write(b"content")?;
    cs.register_name("sess-scope-test", "f.txt", &h)?;

    let build_args = serde_json::json!({ "inputs": ["f.txt"] });
    let build_out = registry.execute(
        "artifact.build",
        &writer,
        &writer_policy,
        &agent_dir,
        Some(&gateway_dir),
        &build_args.to_string(),
        Some("sess-scope-test"),
        None,
        Some(&config),
        Some(store.clone()),
        None,
    )?;
    let build_v: serde_json::Value = serde_json::from_str(&build_out)?;
    let artifact_ref = build_v["artifact_ref"].as_str().expect("artifact_ref");

    let reader = reader_manifest();
    let reader_policy = PolicyEngine::new(reader.clone());

    let resolve_args = serde_json::json!({
        "ref_id": artifact_ref,
        "scope_type": "session",
        "scope_id": "wrong-session-id"
    });
    let result = registry.execute(
        "artifact.resolve_ref",
        &reader,
        &reader_policy,
        &agent_dir,
        Some(&gateway_dir),
        &resolve_args.to_string(),
        Some("sess-scope-test"),
        None,
        Some(&config),
        Some(store.clone()),
        None,
    );

    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(err.contains("not found") || err.contains("expired/revoked"));
    Ok(())
}

#[test]
fn test_artifact_resolve_ref_missing_ref_fails() -> anyhow::Result<()> {
    let temp = tempdir()?;
    let agents_dir = temp.path().join("agents");
    let gateway_dir = agents_dir.join(".gateway");
    std::fs::create_dir_all(&gateway_dir)?;

    let config = GatewayConfig {
        agents_dir: agents_dir.clone(),
        ..GatewayConfig::default()
    };

    let store = Arc::new(GatewayStore::open(&gateway_dir)?);
    let reader = reader_manifest();
    let reader_policy = PolicyEngine::new(reader.clone());
    let registry = default_registry();

    let agent_dir = agents_dir.join("evaluator.default");
    std::fs::create_dir_all(&agent_dir)?;

    let resolve_args = serde_json::json!({
        "ref_id": "ar.nonexistent",
        "scope_type": "session",
        "scope_id": "any-session"
    });
    let result = registry.execute(
        "artifact.resolve_ref",
        &reader,
        &reader_policy,
        &agent_dir,
        Some(&gateway_dir),
        &resolve_args.to_string(),
        Some("any-session"),
        None,
        Some(&config),
        Some(store.clone()),
        None,
    );

    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(err.contains("not found"));
    Ok(())
}

#[test]
fn test_artifact_resolve_ref_expired_ref_fails() -> anyhow::Result<()> {
    use autonoetic_types::artifact::ArtifactRefRecord;
    use chrono::{Duration, Utc};

    let temp = tempdir()?;
    let agents_dir = temp.path().join("agents");
    let gateway_dir = agents_dir.join(".gateway");
    std::fs::create_dir_all(&gateway_dir)?;

    let config = GatewayConfig {
        agents_dir: agents_dir.clone(),
        ..GatewayConfig::default()
    };

    let store = Arc::new(GatewayStore::open(&gateway_dir)?);

    let writer = writer_manifest();
    let writer_policy = PolicyEngine::new(writer.clone());
    let registry = default_registry();

    let agent_dir = agents_dir.join("coder.default");
    std::fs::create_dir_all(&agent_dir)?;

    let cs = ContentStore::new(&gateway_dir)?;
    let h = cs.write(b"expiring")?;
    cs.register_name("sess-expired", "e.txt", &h)?;

    let build_args = serde_json::json!({ "inputs": ["e.txt"] });
    let build_out = registry.execute(
        "artifact.build",
        &writer,
        &writer_policy,
        &agent_dir,
        Some(&gateway_dir),
        &build_args.to_string(),
        Some("sess-expired"),
        None,
        Some(&config),
        Some(store.clone()),
        None,
    )?;
    let build_v: serde_json::Value = serde_json::from_str(&build_out)?;
    let artifact_ref = build_v["artifact_ref"].as_str().expect("artifact_ref");
    let artifact_id = build_v["artifact_id"].as_str().expect("artifact_id");
    let digest = build_v["digest"].as_str().expect("digest");

    let expired_record = ArtifactRefRecord {
        ref_id: "ar.expired.001".to_string(),
        scope_type: ArtifactRefScopeType::Session,
        scope_id: "sess-expired".to_string(),
        artifact_id: artifact_id.to_string(),
        artifact_digest: digest.to_string(),
        created_by_agent_id: "coder.default".to_string(),
        created_at: Utc::now().to_rfc3339(),
        expires_at: Some((Utc::now() - Duration::seconds(10)).to_rfc3339()),
        revoked_at: None,
    };
    store.create_artifact_ref(&expired_record)?;

    let reader = reader_manifest();
    let reader_policy = PolicyEngine::new(reader.clone());

    let resolve_args = serde_json::json!({
        "ref_id": "ar.expired.001",
        "scope_type": "session",
        "scope_id": "sess-expired"
    });
    let result = registry.execute(
        "artifact.resolve_ref",
        &reader,
        &reader_policy,
        &agent_dir,
        Some(&gateway_dir),
        &resolve_args.to_string(),
        Some("sess-expired"),
        None,
        Some(&config),
        Some(store.clone()),
        None,
    );

    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(err.contains("not found") || err.contains("expired/revoked"));
    Ok(())
}

#[test]
fn test_artifact_resolve_ref_revoked_ref_fails() -> anyhow::Result<()> {
    use autonoetic_types::artifact::ArtifactRefRecord;
    use chrono::Utc;

    let temp = tempdir()?;
    let agents_dir = temp.path().join("agents");
    let gateway_dir = agents_dir.join(".gateway");
    std::fs::create_dir_all(&gateway_dir)?;

    let config = GatewayConfig {
        agents_dir: agents_dir.clone(),
        ..GatewayConfig::default()
    };

    let store = Arc::new(GatewayStore::open(&gateway_dir)?);

    let writer = writer_manifest();
    let writer_policy = PolicyEngine::new(writer.clone());
    let registry = default_registry();

    let agent_dir = agents_dir.join("coder.default");
    std::fs::create_dir_all(&agent_dir)?;

    let cs = ContentStore::new(&gateway_dir)?;
    let h = cs.write(b"revoked")?;
    cs.register_name("sess-revoked", "r.txt", &h)?;

    let build_args = serde_json::json!({ "inputs": ["r.txt"] });
    let build_out = registry.execute(
        "artifact.build",
        &writer,
        &writer_policy,
        &agent_dir,
        Some(&gateway_dir),
        &build_args.to_string(),
        Some("sess-revoked"),
        None,
        Some(&config),
        Some(store.clone()),
        None,
    )?;
    let build_v: serde_json::Value = serde_json::from_str(&build_out)?;
    let artifact_id = build_v["artifact_id"].as_str().expect("artifact_id");
    let digest = build_v["digest"].as_str().expect("digest");

    let revoked_record = ArtifactRefRecord {
        ref_id: "ar.revoked.001".to_string(),
        scope_type: ArtifactRefScopeType::Session,
        scope_id: "sess-revoked".to_string(),
        artifact_id: artifact_id.to_string(),
        artifact_digest: digest.to_string(),
        created_by_agent_id: "coder.default".to_string(),
        created_at: Utc::now().to_rfc3339(),
        expires_at: None,
        revoked_at: Some(Utc::now().to_rfc3339()),
    };
    store.create_artifact_ref(&revoked_record)?;

    let reader = reader_manifest();
    let reader_policy = PolicyEngine::new(reader.clone());

    let resolve_args = serde_json::json!({
        "ref_id": "ar.revoked.001",
        "scope_type": "session",
        "scope_id": "sess-revoked"
    });
    let result = registry.execute(
        "artifact.resolve_ref",
        &reader,
        &reader_policy,
        &agent_dir,
        Some(&gateway_dir),
        &resolve_args.to_string(),
        Some("sess-revoked"),
        None,
        Some(&config),
        Some(store.clone()),
        None,
    );

    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(err.contains("not found") || err.contains("expired/revoked"));
    Ok(())
}
