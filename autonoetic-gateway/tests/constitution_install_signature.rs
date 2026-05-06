//! Constitution R+11: Gateway auto-signing of agent revision bundles.
//!
//! The gateway automatically signs every revision with its Ed25519 identity key
//! at creation time. The signature covers the full canonical content digest
//! (all files: SKILL.md + runtime.lock + artifact files). The signer_id field
//! records which key produced the signature, enabling key-agnostic verification
//! for future federation/external signer support.
//!
//! `trust_unsigned_bundles: true` is an escape hatch for environments where
//! the gateway identity key cannot be loaded (e.g. permission errors on the
//! private key file). In normal operation, the gateway always auto-signs.

mod support;

use autonoetic_gateway::runtime::crypto::GatewayIdentityKey;
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

fn build_test_artifact(gw_dir: &std::path::Path) -> String {
    let artifact_store = autonoetic_gateway::ArtifactStore::new(gw_dir).unwrap();
    let skill_md = "---\nname: test\ndescription: test agent\nmetadata:\n  autonoetic:\n    version: \"1.0\"\n    runtime:\n      engine: autonoetic\n      gateway_version: \"0.1.0\"\n      sdk_version: \"0.1.0\"\n      type: stateful\n      sandbox: bubblewrap\n      runtime_lock: runtime.lock\n    agent:\n      id: revision.tester\n      name: revision.tester\n      description: test\n    capabilities:\n      - type: AgentRevision\n        patterns: [\"*\"]\n    execution_mode: reasoning\n---\n\nTest instructions.\n";
    let runtime_lock = r#"{"gateway":{"artifact":"marketplace://gateway/autonoetic-gateway","version":"0.1.0","sha256":"replace-me"},"sdk":{"version":"0.1.0"},"sandbox":{"backend":"bubblewrap"},"dependencies":[],"artifacts":[],"layers":[]}"#;

    let session_id = "test-session-signature";
    let content_store =
        autonoetic_gateway::runtime::content_store::ContentStore::new(gw_dir).unwrap();
    let h1 = content_store.write(skill_md.as_bytes()).unwrap();
    content_store.register_name(session_id, "SKILL.md", &h1).unwrap();
    let h2 = content_store.write(runtime_lock.as_bytes()).unwrap();
    content_store
        .register_name(session_id, "runtime.lock", &h2)
        .unwrap();

    let bundle = artifact_store
        .build_with_kind(
            &["SKILL.md".to_string(), "runtime.lock".to_string()],
            None,
            None,
            autonoetic_types::artifact::ArtifactKind::AgentBundle,
            session_id,
        )
        .unwrap();
    bundle.artifact_id
}

fn create_test_revision(
    temp: &tempfile::TempDir,
    gw_dir: &std::path::Path,
    store: &Arc<GatewayStore>,
) -> (String, autonoetic_types::agent_revision::AgentRevisionRecord) {
    let manifest = test_manifest();
    let policy = autonoetic_gateway::policy::PolicyEngine::new(manifest.clone());
    let registry = default_registry();
    let config = config_strict();
    let artifact_id = build_test_artifact(gw_dir);

    let args = serde_json::json!({
        "agent_id": "revision.tester",
        "artifact_id": artifact_id,
    });

    let result = registry.execute(
        "agent_revision_create",
        &manifest,
        &policy,
        temp.path(),
        Some(gw_dir),
        &args.to_string(),
        None,
        None,
        Some(&config),
        Some(store.clone()),
        None,
    );

    let output = result.expect("revision create should succeed");
    let parsed: serde_json::Value = serde_json::from_str(&output).unwrap();
    assert!(parsed["ok"].as_bool().unwrap());
    let revision_id = parsed["revision_id"].as_str().unwrap().to_string();
    let rev = store
        .get_agent_revision(&revision_id)
        .unwrap()
        .expect("revision should exist");
    (revision_id, rev)
}

#[test]
fn r11_config_default_is_strict() {
    let config = GatewayConfig::default();
    assert!(
        !config.trust_unsigned_bundles,
        "default should not trust unsigned bundles"
    );
}

#[test]
fn r11_auto_signs_revision_on_create() {
    let temp = tempdir().unwrap();
    let (gw_dir, store) = setup_gateway(temp.path());
    let (_revision_id, rev) = create_test_revision(&temp, &gw_dir, &store);

    assert!(rev.signature.is_some(), "revision should have a signature");
    assert!(rev.signer_id.is_some(), "revision should have a signer_id");
    assert!(
        rev.signer_id.as_ref().unwrap().starts_with("gateway:"),
        "signer_id should start with 'gateway:'"
    );
}

#[test]
fn r11_auto_signature_verifies_against_gateway_key() {
    let temp = tempdir().unwrap();
    let (gw_dir, store) = setup_gateway(temp.path());
    let key = GatewayIdentityKey::load_or_generate(&gw_dir).unwrap();
    let pub_bytes = key.public_key_bytes();

    let (revision_id, rev) = create_test_revision(&temp, &gw_dir, &store);
    let sig_b64 = rev.signature.as_ref().expect("should have signature");
    let digest_hex = revision_id.strip_prefix("rev_sha256:").unwrap();

    assert!(
        autonoetic_gateway::runtime::crypto::ManifestVerifier::verify(
            &pub_bytes,
            digest_hex,
            sig_b64,
        )
        .unwrap(),
        "auto-generated signature should verify against gateway public key"
    );
    assert_eq!(
        rev.signer_id.as_ref().unwrap(),
        &format!("gateway:{}", key.fingerprint())
    );
}

#[test]
fn r11_tampered_digest_does_not_verify() {
    let temp = tempdir().unwrap();
    let (gw_dir, store) = setup_gateway(temp.path());
    let key = GatewayIdentityKey::load_or_generate(&gw_dir).unwrap();
    let pub_bytes = key.public_key_bytes();

    let (_revision_id, rev) = create_test_revision(&temp, &gw_dir, &store);
    let sig_b64 = rev.signature.as_ref().unwrap();

    let tampered_digest = "deadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef";
    assert!(
        !autonoetic_gateway::runtime::crypto::ManifestVerifier::verify(
            &pub_bytes,
            tampered_digest,
            sig_b64,
        )
        .unwrap(),
        "tampered digest should not verify"
    );
}

#[test]
fn r11_wrong_key_does_not_verify() {
    let temp = tempdir().unwrap();
    let (gw_dir, store) = setup_gateway(temp.path());

    let (_revision_id, rev) = create_test_revision(&temp, &gw_dir, &store);
    let sig_b64 = rev.signature.as_ref().unwrap();
    let digest_hex = rev.revision_id.strip_prefix("rev_sha256:").unwrap();

    let temp2 = tempdir().unwrap();
    let gw_dir2 = temp2.path().join(".gateway");
    std::fs::create_dir_all(&gw_dir2).unwrap();
    let key2 = GatewayIdentityKey::load_or_generate(&gw_dir2).unwrap();
    let wrong_pub = key2.public_key_bytes();

    assert!(
        !autonoetic_gateway::runtime::crypto::ManifestVerifier::verify(
            &wrong_pub,
            digest_hex,
            sig_b64,
        )
        .unwrap(),
        "signature should not verify against a different gateway key"
    );
}

#[test]
fn r11_response_includes_signed_by_when_signed() {
    let temp = tempdir().unwrap();
    let (gw_dir, store) = setup_gateway(temp.path());
    let manifest = test_manifest();
    let policy = autonoetic_gateway::policy::PolicyEngine::new(manifest.clone());
    let registry = default_registry();
    let config = config_strict();
    let artifact_id = build_test_artifact(&gw_dir);

    let args = serde_json::json!({
        "agent_id": "revision.tester",
        "artifact_id": artifact_id,
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

    let output = result.expect("should succeed");
    let parsed: serde_json::Value = serde_json::from_str(&output).unwrap();
    assert!(
        parsed.get("signed_by").is_some(),
        "response should include signed_by field when signed"
    );
    let signed_by = parsed["signed_by"].as_str().unwrap();
    assert!(
        signed_by.starts_with("gateway:"),
        "signed_by should start with 'gateway:'"
    );
}
