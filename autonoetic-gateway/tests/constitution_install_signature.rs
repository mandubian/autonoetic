//! Constitution R+11 / R-9.13: Bundle signature verification at revision create.
//!
//! When `trust_unsigned_bundles` is false (the default in production), every
//! `agent_revision_create` must carry a valid Ed25519 signature over the
//! canonical content digest. Unsigned bundles are rejected.
//!
//! When `trust_unsigned_bundles` is true (dev mode), signatures are optional.

mod support;

use autonoetic_gateway::runtime::crypto::{ManifestSigner, ManifestVerifier};
use autonoetic_gateway::runtime::tools::default_registry;
use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_types::agent::{AgentIdentity, AgentManifest, RuntimeDeclaration};
use autonoetic_types::capability::Capability;
use autonoetic_types::config::GatewayConfig;
use std::sync::Arc;
use tempfile::tempdir;

fn test_manifest() -> AgentManifest {
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
            id: "revision.tester".to_string(),
            name: "revision.tester".to_string(),
            description: "test".to_string(),
        },
        capabilities: vec![Capability::AgentRevision { patterns: vec!["*".to_string()] }],
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
        response_contract: None,
        allowed_tool_tiers: vec![],
        agentskills_import: None,
        compression: None,
    }
}

fn setup_gateway(base: &std::path::Path) -> (std::path::PathBuf, Arc<GatewayStore>) {
    let gw_dir = base.join(".gateway");
    std::fs::create_dir_all(&gw_dir).unwrap();
    let store = Arc::new(GatewayStore::open(&gw_dir).unwrap());
    (gw_dir, store)
}

fn config_strict() -> GatewayConfig {
    let mut c = GatewayConfig::default();
    c.trust_unsigned_bundles = false;
    c
}

fn config_trust() -> GatewayConfig {
    let mut c = GatewayConfig::default();
    c.trust_unsigned_bundles = true;
    c
}

#[test]
fn r11_sign_and_verify_roundtrip() {
    let secret = [42u8; 32];
    let signer = ManifestSigner::new(&secret);
    let content = "test bundle content digest hex";
    let sig = signer.sign(content);

    let public_bytes = signer.public_key_bytes();
    assert!(
        ManifestVerifier::verify(&public_bytes, content, &sig).unwrap(),
        "signature should verify"
    );
    assert!(
        !ManifestVerifier::verify(&public_bytes, "tampered content", &sig).unwrap(),
        "tampered content should not verify"
    );
}

#[test]
fn r11_config_default_is_strict() {
    let config = GatewayConfig::default();
    assert!(
        !config.trust_unsigned_bundles,
        "default should require signatures"
    );
}

#[test]
fn r11_revision_create_rejects_unsigned_when_strict() {
    let temp = tempdir().unwrap();
    let (gw_dir, store) = setup_gateway(temp.path());
    let manifest = test_manifest();
    let policy = autonoetic_gateway::policy::PolicyEngine::new(manifest.clone());
    let registry = default_registry();
    let config = config_strict();

    let args = serde_json::json!({
        "agent_id": "revision.tester",
        "artifact_id": "nonexistent",
    });

    let result = registry.execute(
        "agent_revision_create",
        &manifest,
        &policy,
        temp.path(),
        Some(&gw_dir),
        &args.to_string(),
        None,
        None,
        Some(&config),
        Some(store),
        None,
    );

    let err = result.expect_err("should reject unsigned bundle");
    assert!(
        err.to_string().contains("R+11"),
        "error should reference R+11: {}",
        err
    );
}

#[test]
fn r11_revision_create_allows_unsigned_when_trusted() {
    let temp = tempdir().unwrap();
    let (gw_dir, store) = setup_gateway(temp.path());
    let manifest = test_manifest();
    let policy = autonoetic_gateway::policy::PolicyEngine::new(manifest.clone());
    let registry = default_registry();
    let config = config_trust();

    let args = serde_json::json!({
        "agent_id": "revision.tester",
        "artifact_id": "nonexistent",
    });

    let result = registry.execute(
        "agent_revision_create",
        &manifest,
        &policy,
        temp.path(),
        Some(&gw_dir),
        &args.to_string(),
        None,
        None,
        Some(&config),
        Some(store),
        None,
    );

    // It won't succeed (artifact doesn't exist), but it should NOT fail
    // with the R+11 signature gate — the error should be about the missing
    // artifact, not about signatures.
    match result {
        Ok(_) => {}
        Err(e) => {
            assert!(
                !e.to_string().contains("R+11"),
                "should not fail with R+11 signature gate when trust_unsigned_bundles is true: {}",
                e
            );
        }
    }
}
