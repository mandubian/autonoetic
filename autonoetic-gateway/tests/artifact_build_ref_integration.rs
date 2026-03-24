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
    )?;
    let v2: serde_json::Value = serde_json::from_str(&out2)?;
    assert_eq!(v2.get("reused"), Some(&serde_json::json!(true)));
    assert!(v2.get("artifact_ref").is_none());
    assert_eq!(v1["artifact_id"], v2["artifact_id"]);
    Ok(())
}
