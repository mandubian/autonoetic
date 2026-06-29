mod support;

use autonoetic_gateway::policy::PolicyEngine;
use autonoetic_gateway::runtime::content_store::ContentStore;
use autonoetic_gateway::runtime::tools::default_registry;
use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_types::agent::{AgentIdentity, AgentManifest, RuntimeDeclaration};
use autonoetic_types::artifact::{ArtifactKind, ArtifactRefScopeType};
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
  description: "Zero-capability test agent"
execution_mode: script
script_entry: main.py
---
# Test agent
"#
    )
}

fn manifest_for(agent_id: &str) -> AgentManifest {
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
            singleton: false,
        },
        capabilities: vec![Capability::AgentRevision {
            patterns: vec!["*".to_string()],
        }],
        llm_overrides: None,
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
            open_web: false,
        sandbox_network: autonoetic_types::agent::SandboxNetworkPolicy::default(),
    }
}

fn build_agent_bundle(base_dir: &Path, skill_md: &str) -> (String, PathBuf) {
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
    (bundle.artifact_id, gateway_dir)
}

use std::path::PathBuf;

#[test]
fn promotion_upgrades_artifact_ref_to_global_scope() {
    let agent_id = "test.global-ref-agent";
    let skill = zero_cap_skill_md(agent_id);
    let temp = tempdir().unwrap();
    let agents_dir = temp.path().join("agents");
    let builder_dir = agents_dir.join("planner.default");
    std::fs::create_dir_all(&builder_dir).unwrap();

    let (artifact_id, gateway_dir) = build_agent_bundle(temp.path(), &skill);
    let config = GatewayConfig {
        agents_dir,
        ..Default::default()
    };
    let store = Arc::new(GatewayStore::open(&gateway_dir).unwrap());
    let registry = default_registry();

    let builder = manifest_for("planner.default");
    let builder_policy = PolicyEngine::new(builder.clone());

    // Create a session-scoped artifact ref for the artifact
    let ref_id = {
        let ref_record = autonoetic_types::artifact::ArtifactRefRecord {
            ref_id: "ar.aabbccddeeff".to_string(),
            scope_type: ArtifactRefScopeType::Session,
            scope_id: "session-builder".to_string(),
            artifact_id: artifact_id.clone(),
            artifact_manifest_digest: "sha256:dummy".to_string(),
            artifact_canonical_digest: "sha256:dummy".to_string(),
            created_by_agent_id: "planner.default".to_string(),
            created_at: chrono::Utc::now().to_rfc3339(),
            expires_at: None,
            revoked_at: None,
        };
        store.create_artifact_ref(&ref_record).unwrap();
        ref_record.ref_id
    };

    // Verify: resolvable from builder session
    let resolved = store
        .resolve_artifact_ref(ArtifactRefScopeType::Session, "session-builder", &ref_id)
        .unwrap();
    assert!(resolved.is_some(), "ref should be resolvable from builder session");

    // Verify: NOT resolvable from a different session
    let resolved_other = store
        .resolve_artifact_ref_any_scope(&ref_id, "session-other")
        .unwrap();
    assert!(
        resolved_other.is_none(),
        "ref should NOT be resolvable from a different session before promotion"
    );

    // Create revision from artifact
    let rev_args = serde_json::json!({
        "agent_id": agent_id,
        "artifact_id": artifact_id,
    });
    let rev_result = registry
        .execute(
            "agent_revision_create",
            &builder,
            &builder_policy,
            &builder_dir,
            Some(&gateway_dir),
            &serde_json::to_string(&rev_args).unwrap(),
            Some("session-builder"),
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
        .expect("revision_id in response")
        .to_string();

    // Promote the revision (zero-capability agent → direct promote, no gate)
    let promote_args = serde_json::json!({
        "agent_id": agent_id,
        "revision_id": revision_id,
        "reason": "integration test — verify global ref upgrade",
    });
    let promote_result = registry
        .execute(
            "agent_revision_promote",
            &builder,
            &builder_policy,
            &builder_dir,
            Some(&gateway_dir),
            &serde_json::to_string(&promote_args).unwrap(),
            Some("session-builder"),
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
        "promote should return ok=true, got: {promote_result}"
    );

    // After promotion: the ref should now be resolvable from ANY session
    let resolved_global = store
        .resolve_artifact_ref_any_scope(&ref_id, "session-other")
        .unwrap();
    assert!(
        resolved_global.is_some(),
        "ref should be resolvable from a different session after promotion (global scope)"
    );
    let record = resolved_global.unwrap();
    assert_eq!(
        record.scope_type,
        ArtifactRefScopeType::Global,
        "ref scope should be Global after promotion"
    );
    assert_eq!(record.ref_id, ref_id);
    assert_eq!(record.artifact_id, artifact_id);
}
