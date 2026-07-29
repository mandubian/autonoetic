//! Historical coverage targeted `agent.install` + human approval. `agent.install` has been removed from native tools.
//! Approval queues for other tools (e.g. sandbox.exec) remain covered in
//! `turn_continuation_approval_integration` and related tests.

use autonoetic_gateway::policy::PolicyEngine;
use autonoetic_gateway::runtime::content_store::ContentStore;
use autonoetic_gateway::runtime::tools::default_registry;
use autonoetic_types::agent::{AgentIdentity, AgentManifest};
use autonoetic_types::capability::Capability;
use autonoetic_types::config::GatewayConfig;
use tempfile::tempdir;
use crate::support::manifest_builder::TestManifest;

fn evolution_manifest() -> AgentManifest {
    AgentManifest {
        agent: AgentIdentity {
            id: "specialized_builder.default".to_string(),
            name: "specialized_builder.default".to_string(),
            description: "Builder".to_string(),
            singleton: false,
            resident_idle_ttl_secs: None,
        },
        capabilities: vec![Capability::AgentSpawn {
            max_children: 10,
            max_spawn_depth: 0,
        }],
        ..TestManifest::new().build()
    }
}

#[tokio::test]
async fn test_agent_install_no_longer_uses_install_approval_flow() {
    let temp = tempdir().expect("tempdir");
    let agents_dir = temp.path().join("agents");
    let gateway_dir = agents_dir.join(".gateway");
    let builder_dir = agents_dir.join("specialized_builder.default");
    std::fs::create_dir_all(&builder_dir).unwrap();
    std::fs::create_dir_all(&gateway_dir).unwrap();

    let config = GatewayConfig {
        agents_dir: agents_dir.clone(),
        ..Default::default()
    };

    let cs = ContentStore::new(&gateway_dir).unwrap();
    let h = cs.write(b"x").unwrap();
    cs.register_name("s", "f.txt", &h).unwrap();
    let art = autonoetic_gateway::artifact_store::ArtifactStore::new(&gateway_dir).unwrap();
    let bundle = art.build(&["f.txt".to_string()], None, None, "s").unwrap();

    let args = serde_json::json!({
        "agent_id": "legacy",
        "instructions": "# x",
        "artifact_id": bundle.artifact_id,
        "capabilities": [{ "type": "NetworkAccess", "hosts": ["*"] }],
        "promotion_gate": {
            "evaluator_pass": true,
            "auditor_pass": true,
            "security_analysis": { "passed": true, "threats_detected": [], "remote_access_detected": true },
            "capability_analysis": {
                "inferred_capabilities": ["NetworkAccess"],
                "missing_capabilities": [],
                "declared_capabilities": ["NetworkAccess"],
                "analysis_passed": true
            }
        }
    });

    let err = default_registry()
        .execute(
            "agent.install",
            &evolution_manifest(),
            &PolicyEngine::new(evolution_manifest()),
            &builder_dir,
            Some(&gateway_dir),
            &serde_json::to_string(&args).unwrap(),
            Some("sid"),
            None,
            Some(&config),
            None,
            None,
        )
        .expect_err("unavailable");

    assert!(err.to_string().contains("Unknown native tool"), "{}", err);
}
